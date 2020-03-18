use super::*;
use crate::client::Context;
use crate::model::channel::Message;
use uwl::Stream;

pub mod map;

use map::{CommandMap, GroupMap, ParseMap};

use std::borrow::Cow;
#[cfg(feature = "cache")]
use futures::future::BoxFuture;
use futures::future::FutureExt;

#[inline]
fn to_lowercase<'a>(config: &Configuration, s: &'a str) -> Cow<'a, str> {
    if config.case_insensitive {
        Cow::Owned(s.to_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// Parse a mention in the message that is of either the direct (`<@id>`) or nickname (`<@!id>`) syntax,
/// and compare the encoded `id` with the id from [`Configuration::on_mention`] for a match.
/// Returns `Some(<id>)` on success, `None` otherwise.
///
/// [`Configuration::on_mention`]: ../struct.Configuration.html#method.on_mention
pub fn mention<'a>(stream: &mut Stream<'a>, config: &Configuration) -> Option<&'a str> {
    let on_mention = config.on_mention.as_ref().map(String::as_str)?;

    let start = stream.offset();

    if !stream.eat("<@") {
        return None;
    }

    // Optional.
    stream.eat("!");

    let id = stream.take_while(|s| s.is_numeric());

    if !stream.eat(">") {
        // Backtrack to where we were.
        stream.set(start);

        return None;
    }

    if id == on_mention {
        Some(id)
    } else {
        stream.set(start);

        None
    }
}

async fn find_prefix<'a>(
    ctx: &mut Context,
    msg: &Message,
    config: &Configuration,
    stream: &Stream<'a>,
) -> Option<Cow<'a, str>> {
    let try_match = |prefix: &str| {
        let peeked = stream.peek_for(prefix.chars().count());
        let peeked = to_lowercase(config, peeked);

        if prefix == &peeked {
            Some(peeked)
        } else {
            None
        }
    };

    for f in &config.dynamic_prefixes {
        if let Some(p) = f(ctx, msg).await {
            if let Some(p) = try_match(&p) {
                return Some(p);
            }
        }
    }

    config.prefixes.iter().find_map(|p| try_match(&p))
}

/// Parse a prefix in the message.
///
/// The "prefix" may be one of the following:
/// - A mention (`<@id>`/`<@!id>`)
/// - A dynamically constructed prefix ([`Configuration::dynamic_prefix`])
/// - A static prefix ([`Configuration::prefix`])
/// - Nothing
///
/// In all cases, whitespace after the prefix is cleared.
///
/// [`Configuration::dynamic_prefix`]: ../struct.Configuration.html#method.dynamic_prefix
/// [`Configuration::prefix`]: ../struct.Configuration.html#method.prefix
pub async fn prefix<'a>(
    ctx: &mut Context,
    msg: &Message,
    stream: &mut Stream<'a>,
    config: &Configuration,
) -> Option<Cow<'a, str>> {
    if let Some(id) = mention(stream, config) {
        stream.take_while(|s| s.is_whitespace());

        return Some(Cow::Borrowed(id));
    }

    let prefix = find_prefix(ctx, msg, config, stream).await;

    if let Some(prefix) = &prefix {
        stream.increment(prefix.len());
    }

    if config.with_whitespace.prefixes {
        stream.take_while(|s| s.is_whitespace());
    }

    prefix
}

/// Checked per valid group or command in the message.
async fn check_discrepancy(
    _ctx: &Context,
    msg: &Message,
    config: &Configuration,
    options: &impl CommonOptions,
) -> Result<(), DispatchError> {
    if options.owners_only() && !config.owners.contains(&msg.author.id) {
        return Err(DispatchError::OnlyForOwners);
    }

    if options.only_in() == OnlyIn::Dm && !msg.is_private() {
        return Err(DispatchError::OnlyForDM);
    }

    if (!config.allow_dm || options.only_in() == OnlyIn::Guild) && msg.is_private() {
        return Err(DispatchError::OnlyForGuilds);
    }

    #[cfg(feature = "cache")]
    {
        if let Some(guild_id) = msg.guild_id {
            let guild = match guild_id.to_guild_cached(&_ctx).await {
                Some(g) => g,
                None => return Ok(()),
            };

            let guild = guild.read().await;

            let perms = guild.user_permissions_in(msg.channel_id, msg.author.id).await;

            if !perms.contains(*options.required_permissions())
                && !(options.owner_privilege() && config.owners.contains(&msg.author.id))
            {
                return Err(DispatchError::LackingPermissions(
                    *options.required_permissions(),
                ));
            }

            if let Some(member) = guild.members.get(&msg.author.id) {
                if !perms.administrator() && !has_correct_roles(options, &guild, &member) {
                    return Err(DispatchError::LackingRole);
                }
            }
        }
    }

    Ok(())
}

fn try_parse<M: ParseMap>(
    stream: &mut Stream<'_>,
    map: &M,
    by_space: bool,
    f: impl Fn(&str) -> String,
) -> (String, Option<M::Storage>) {
    if by_space {
        let n = f(stream.peek_until(|s| s.is_whitespace()));

        let o = map.get(&n);

        (n, o)
    } else {
        let mut n = f(stream.peek_for(map.max_length()));
        let mut o = None;

        for _ in 0..(map.max_length() - map.min_length()) {
            o = map.get(&n);

            if o.is_some() {
                break;
            }

            n.pop();
        }

        (n, o)
    }
}

// Recursive function returning the actual async logic.
fn parse_cmd<'a>(
    stream: &'a mut Stream<'_>,
    ctx: &'a Context,
    msg: &'a Message,
    config: &'a Configuration,
    map: &'a CommandMap,
) -> BoxFuture<'a, Result<&'static Command, ParseError>> {
    async move {
        let (n, r) = try_parse(stream, map, config.by_space, |s| {
            to_lowercase(config, s).into_owned()
        });

        if config.disabled_commands.contains(&n) {
            return Err(ParseError::Dispatch(DispatchError::CommandDisabled(n)));
        }

        if let Some((cmd, map)) = r {
            stream.increment(n.len());

            if config.with_whitespace.commands {
                stream.take_while(|s| s.is_whitespace());
            }

            check_discrepancy(ctx, msg, config, &cmd.options).await?;

            if map.is_empty() {
                return Ok(cmd);
            }

            return match parse_cmd(stream, ctx, msg, config, &map).await {
                Err(ParseError::UnrecognisedCommand(Some(_))) => Ok(cmd),
                res => res,
            };
        }

        Err(ParseError::UnrecognisedCommand(Some(n.to_string())))
    }.boxed()
}


// Recursive blocking function that calls its async variant.
fn parse_group<'a>(
    stream: &'a mut Stream<'_>,
    ctx: &'a Context,
    msg: &'a Message,
    config: &'a Configuration,
    map: &'a GroupMap,
) -> BoxFuture<'a, Result<(&'static CommandGroup, Arc<CommandMap>), ParseError>> {
    async move {
        let (n, o) = try_parse(stream, map, config.by_space, ToString::to_string);

        if let Some((group, map, commands)) = o {
            stream.increment(n.len());

            if config.with_whitespace.groups {
                stream.take_while(|s| s.is_whitespace());
            }

            check_discrepancy(ctx, msg, config, &group.options).await?;

            if map.is_empty() {
                return Ok((group, commands));
            }

            return match parse_group(stream, ctx, msg, config, &map).await {
                Err(ParseError::UnrecognisedCommand(None)) => Ok((group, commands)),
                res => res,
            };
        }

        Err(ParseError::UnrecognisedCommand(None))
    }.boxed()
}

#[inline]
async fn handle_command<'a>(
    stream: &'a mut Stream<'_>,
    ctx: &'a Context,
    msg: &'a Message,
    config: &'a Configuration,
    map: &'a CommandMap,
    group: &'static CommandGroup,
) -> Result<Invoke, ParseError> {
    match parse_cmd(stream, ctx, msg, config, map).await {
        Ok(command) => Ok(Invoke::Command { group, command }),
        Err(err) => match group.options.default_command {
            Some(command) => Ok(Invoke::Command { group, command }),
            None => Err(err),
        },
    }
}

#[inline]
async fn handle_group(
    stream: &mut Stream<'_>,
    ctx: &Context,
    msg: &Message,
    config: &Configuration,
    map: &GroupMap,
) -> Result<Invoke, ParseError> {
    match parse_group(stream, ctx, msg, config, map).await {
        Ok((group, map)) => handle_command(stream, ctx, msg, config, &map, group).await,
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub enum ParseError {
    UnrecognisedCommand(Option<String>),
    Dispatch(DispatchError),
}

impl From<DispatchError> for ParseError {
    #[inline]
    fn from(err: DispatchError) -> Self {
        ParseError::Dispatch(err)
    }
}

/// Parse a command from the message.
///
/// The "command" may be:
/// 1. A *help command* that provides a friendly browsing interface of all groups and commands,
/// explaining what each of them are, how they are layed out and how to invoke them.
/// There can only one help command registered, but might have many names defined for invocation of itself.
///
/// 2. A command defined under another command or a group, which may also belong to another group and so on.
/// To invoke this command, all names and prefixes of its parent commands and groups must be specified before it.
pub async fn command(
    ctx: &Context,
    msg: &Message,
    stream: &mut Stream<'_>,
    groups: &[(&'static CommandGroup, Map)],
    config: &Configuration,
    help_was_set: Option<&[&'static str]>,
) -> Result<Invoke, ParseError> {
    // Precedence is taken over commands named as one of the help names.
    if let Some(names) = help_was_set {
        for name in names {
            let n = to_lowercase(config, stream.peek_for(name.chars().count()));

            if name == &n {
                stream.increment(n.len());

                stream.take_while(|s| s.is_whitespace());

                return Ok(Invoke::Help(name));
            }
        }
    }

    let mut last = Err(ParseError::UnrecognisedCommand(None));

    for (group, map) in groups {
        match map {
            // Includes [group] itself.
            Map::WithPrefixes(map) => {
                let res = handle_group(stream, ctx, msg, config, map).await;

                if res.is_ok() {
                    return res;
                }

                last = res;
            }
            Map::Prefixless(subgroups, commands) => {
                let res = handle_group(stream, ctx, msg, config, subgroups).await;

                if res.is_ok() {
                    check_discrepancy(ctx, msg, config, &group.options).await?;

                    return res;
                }

                let res = handle_command(stream, ctx, msg, config, commands, group).await;

                if res.is_ok() {
                    check_discrepancy(ctx, msg, config, &group.options).await?;

                    return res;
                }

                last = res;
            }
        }
    }

    last
}

#[derive(Debug)]
pub enum Invoke {
    Command {
        group: &'static CommandGroup,
        command: &'static Command,
    },
    Help(&'static str),
}
