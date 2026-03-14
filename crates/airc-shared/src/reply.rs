//! IRC numeric reply codes per RFC 2812.
//!
//! Each constant is a `u16` matching the three-digit reply code sent from
//! server to client. The [`reply_name`] function maps codes to their
//! human-readable names.

// ===========================================================================
// Connection & Registration (001–005)
// ===========================================================================

/// `001` — Welcome message after successful registration.
pub const RPL_WELCOME: u16 = 1;
/// `002` — Your host info.
pub const RPL_YOURHOST: u16 = 2;
/// `003` — Server creation date.
pub const RPL_CREATED: u16 = 3;
/// `004` — Server name and version info.
pub const RPL_MYINFO: u16 = 4;

// ===========================================================================
// Luser statistics (251–255)
// ===========================================================================

/// `251` — User count summary.
pub const RPL_LUSERCLIENT: u16 = 251;
/// `252` — Number of IRC operators online.
pub const RPL_LUSEROP: u16 = 252;
/// `253` — Number of unknown connections.
pub const RPL_LUSERUNKNOWN: u16 = 253;
/// `254` — Number of channels formed.
pub const RPL_LUSERCHANNELS: u16 = 254;
/// `255` — Client/server count for this server.
pub const RPL_LUSERME: u16 = 255;

// ===========================================================================
// AWAY / ISON / INVITE (301, 303, 305–306, 341)
// ===========================================================================

/// `301` — Away message (returned when messaging an away user, or in WHOIS).
pub const RPL_AWAY: u16 = 301;
/// `303` — ISON reply (space-separated list of online nicks).
pub const RPL_ISON: u16 = 303;
/// `305` — You are no longer marked as being away.
pub const RPL_UNAWAY: u16 = 305;
/// `306` — You have been marked as being away.
pub const RPL_NOWAWAY: u16 = 306;

// ===========================================================================
// WHO / WHOIS (311–319, 352, 315)
// ===========================================================================

/// `311` — WHOIS user info.
pub const RPL_WHOISUSER: u16 = 311;
/// `312` — WHOIS server info.
pub const RPL_WHOISSERVER: u16 = 312;
/// `313` — WHOIS operator status.
pub const RPL_WHOISOPERATOR: u16 = 313;
/// `315` — End of WHO list.
pub const RPL_ENDOFWHO: u16 = 315;
/// `318` — End of WHOIS list.
pub const RPL_ENDOFWHOIS: u16 = 318;
/// `319` — WHOIS channels.
pub const RPL_WHOISCHANNELS: u16 = 319;
/// `320` — WHOIS special/custom info line.
pub const RPL_WHOISSPECIAL: u16 = 320;
/// `352` — WHO reply.
pub const RPL_WHOREPLY: u16 = 352;

// ===========================================================================
// LIST (322–323)
// ===========================================================================

/// `322` — LIST reply (one channel entry).
pub const RPL_LIST: u16 = 322;
/// `323` — End of LIST.
pub const RPL_LISTEND: u16 = 323;

// ===========================================================================
// Channel topic (331–333)
// ===========================================================================

/// `331` — No topic is set.
pub const RPL_NOTOPIC: u16 = 331;
/// `332` — Channel topic.
pub const RPL_TOPIC: u16 = 332;
/// `333` — Topic set by (nick and timestamp).
pub const RPL_TOPICWHOTIME: u16 = 333;

// ===========================================================================
// NAMES (353, 366)
// ===========================================================================

/// `353` — NAMES reply.
pub const RPL_NAMREPLY: u16 = 353;
/// `366` — End of NAMES list.
pub const RPL_ENDOFNAMES: u16 = 366;

// ===========================================================================
// MOTD (372, 375–376)
// ===========================================================================

/// `372` — MOTD body line.
pub const RPL_MOTD: u16 = 372;
/// `375` — Start of MOTD.
pub const RPL_MOTDSTART: u16 = 375;
/// `376` — End of MOTD.
pub const RPL_ENDOFMOTD: u16 = 376;

// ===========================================================================
// Channel mode (324)
// ===========================================================================

/// `324` — Channel mode is.
pub const RPL_CHANNELMODEIS: u16 = 324;

// ===========================================================================
// INVITE (341)
// ===========================================================================

/// `341` — Returned to the inviter to confirm the invitation was sent.
pub const RPL_INVITING: u16 = 341;

// ===========================================================================
// Error replies (401–482)
// ===========================================================================

/// `401` — No such nick/channel.
pub const ERR_NOSUCHNICK: u16 = 401;
/// `403` — No such channel.
pub const ERR_NOSUCHCHANNEL: u16 = 403;
/// `404` — Cannot send to channel.
pub const ERR_CANNOTSENDTOCHAN: u16 = 404;
/// `405` — Too many channels joined.
pub const ERR_TOOMANYCHANNELS: u16 = 405;
/// `421` — Unknown command.
pub const ERR_UNKNOWNCOMMAND: u16 = 421;
/// `431` — No nickname given.
pub const ERR_NONICKNAMEGIVEN: u16 = 431;
/// `432` — Erroneous nickname.
pub const ERR_ERRONEUSNICKNAME: u16 = 432;
/// `433` — Nickname is already in use.
pub const ERR_NICKNAMEINUSE: u16 = 433;
/// `441` — User not in channel (for KICK, etc.).
pub const ERR_USERNOTINCHANNEL: u16 = 441;
/// `442` — You're not on that channel.
pub const ERR_NOTONCHANNEL: u16 = 442;
/// `443` — User is already on channel (INVITE).
pub const ERR_USERONCHANNEL: u16 = 443;
/// `451` — You have not registered.
pub const ERR_NOTREGISTERED: u16 = 451;
/// `461` — Not enough parameters.
pub const ERR_NEEDMOREPARAMS: u16 = 461;
/// `462` — Already registered.
pub const ERR_ALREADYREGISTERED: u16 = 462;
/// `471` — Cannot join channel (+l) — channel is full.
pub const ERR_CHANNELISFULL: u16 = 471;
/// `473` — Cannot join channel (+i) — invite only.
pub const ERR_INVITEONLYCHAN: u16 = 473;
/// `474` — Cannot join channel (banned).
pub const ERR_BANNEDFROMCHAN: u16 = 474;
/// `475` — Cannot join channel (bad key).
pub const ERR_BADCHANNELKEY: u16 = 475;
/// `482` — You're not channel operator.
pub const ERR_CHANOPRIVSNEEDED: u16 = 482;

// ===========================================================================
// Helper
// ===========================================================================

/// Return the human-readable name for a known IRC numeric reply code.
///
/// Unknown codes return `"UNKNOWN"`.
///
/// # Examples
///
/// ```
/// use airc_shared::reply::reply_name;
/// assert_eq!(reply_name(1), "RPL_WELCOME");
/// assert_eq!(reply_name(433), "ERR_NICKNAMEINUSE");
/// assert_eq!(reply_name(9999), "UNKNOWN");
/// ```
pub fn reply_name(code: u16) -> &'static str {
    match code {
        // Registration
        1 => "RPL_WELCOME",
        2 => "RPL_YOURHOST",
        3 => "RPL_CREATED",
        4 => "RPL_MYINFO",

        // Luser
        251 => "RPL_LUSERCLIENT",
        252 => "RPL_LUSEROP",
        253 => "RPL_LUSERUNKNOWN",
        254 => "RPL_LUSERCHANNELS",
        255 => "RPL_LUSERME",

        // WHO/WHOIS
        301 => "RPL_AWAY",
        303 => "RPL_ISON",
        305 => "RPL_UNAWAY",
        306 => "RPL_NOWAWAY",
        311 => "RPL_WHOISUSER",
        312 => "RPL_WHOISSERVER",
        313 => "RPL_WHOISOPERATOR",
        315 => "RPL_ENDOFWHO",
        318 => "RPL_ENDOFWHOIS",
        319 => "RPL_WHOISCHANNELS",
        320 => "RPL_WHOISSPECIAL",
        352 => "RPL_WHOREPLY",

        // LIST
        322 => "RPL_LIST",
        323 => "RPL_LISTEND",

        // Topic
        331 => "RPL_NOTOPIC",
        332 => "RPL_TOPIC",
        333 => "RPL_TOPICWHOTIME",

        // Channel mode
        324 => "RPL_CHANNELMODEIS",

        // INVITE
        341 => "RPL_INVITING",

        // NAMES
        353 => "RPL_NAMREPLY",
        366 => "RPL_ENDOFNAMES",

        // MOTD
        372 => "RPL_MOTD",
        375 => "RPL_MOTDSTART",
        376 => "RPL_ENDOFMOTD",

        // Errors
        401 => "ERR_NOSUCHNICK",
        403 => "ERR_NOSUCHCHANNEL",
        404 => "ERR_CANNOTSENDTOCHAN",
        405 => "ERR_TOOMANYCHANNELS",
        421 => "ERR_UNKNOWNCOMMAND",
        431 => "ERR_NONICKNAMEGIVEN",
        432 => "ERR_ERRONEUSNICKNAME",
        433 => "ERR_NICKNAMEINUSE",
        441 => "ERR_USERNOTINCHANNEL",
        442 => "ERR_NOTONCHANNEL",
        443 => "ERR_USERONCHANNEL",
        451 => "ERR_NOTREGISTERED",
        461 => "ERR_NEEDMOREPARAMS",
        462 => "ERR_ALREADYREGISTERED",
        471 => "ERR_CHANNELISFULL",
        473 => "ERR_INVITEONLYCHAN",
        474 => "ERR_BANNEDFROMCHAN",
        475 => "ERR_BADCHANNELKEY",
        482 => "ERR_CHANOPRIVSNEEDED",

        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_have_correct_values() {
        assert_eq!(RPL_WELCOME, 1);
        assert_eq!(RPL_YOURHOST, 2);
        assert_eq!(RPL_CREATED, 3);
        assert_eq!(RPL_MYINFO, 4);
        assert_eq!(RPL_LUSERCLIENT, 251);
        assert_eq!(RPL_LUSEROP, 252);
        assert_eq!(RPL_LUSERUNKNOWN, 253);
        assert_eq!(RPL_LUSERCHANNELS, 254);
        assert_eq!(RPL_LUSERME, 255);
        assert_eq!(RPL_AWAY, 301);
        assert_eq!(RPL_ISON, 303);
        assert_eq!(RPL_UNAWAY, 305);
        assert_eq!(RPL_NOWAWAY, 306);
        assert_eq!(RPL_WHOISUSER, 311);
        assert_eq!(RPL_WHOISSERVER, 312);
        assert_eq!(RPL_WHOISOPERATOR, 313);
        assert_eq!(RPL_ENDOFWHO, 315);
        assert_eq!(RPL_ENDOFWHOIS, 318);
        assert_eq!(RPL_WHOISCHANNELS, 319);
        assert_eq!(RPL_WHOISSPECIAL, 320);
        assert_eq!(RPL_LIST, 322);
        assert_eq!(RPL_LISTEND, 323);
        assert_eq!(RPL_NOTOPIC, 331);
        assert_eq!(RPL_TOPIC, 332);
        assert_eq!(RPL_TOPICWHOTIME, 333);
        assert_eq!(RPL_CHANNELMODEIS, 324);
        assert_eq!(RPL_INVITING, 341);
        assert_eq!(RPL_WHOREPLY, 352);
        assert_eq!(RPL_NAMREPLY, 353);
        assert_eq!(RPL_ENDOFNAMES, 366);
        assert_eq!(RPL_MOTD, 372);
        assert_eq!(RPL_MOTDSTART, 375);
        assert_eq!(RPL_ENDOFMOTD, 376);
        assert_eq!(ERR_NOSUCHNICK, 401);
        assert_eq!(ERR_NOSUCHCHANNEL, 403);
        assert_eq!(ERR_CANNOTSENDTOCHAN, 404);
        assert_eq!(ERR_TOOMANYCHANNELS, 405);
        assert_eq!(ERR_UNKNOWNCOMMAND, 421);
        assert_eq!(ERR_NONICKNAMEGIVEN, 431);
        assert_eq!(ERR_ERRONEUSNICKNAME, 432);
        assert_eq!(ERR_NICKNAMEINUSE, 433);
        assert_eq!(ERR_USERNOTINCHANNEL, 441);
        assert_eq!(ERR_NOTONCHANNEL, 442);
        assert_eq!(ERR_USERONCHANNEL, 443);
        assert_eq!(ERR_NOTREGISTERED, 451);
        assert_eq!(ERR_NEEDMOREPARAMS, 461);
        assert_eq!(ERR_ALREADYREGISTERED, 462);
        assert_eq!(ERR_CHANNELISFULL, 471);
        assert_eq!(ERR_INVITEONLYCHAN, 473);
        assert_eq!(ERR_BANNEDFROMCHAN, 474);
        assert_eq!(ERR_BADCHANNELKEY, 475);
        assert_eq!(ERR_CHANOPRIVSNEEDED, 482);
    }

    #[test]
    fn reply_name_known_codes() {
        assert_eq!(reply_name(RPL_WELCOME), "RPL_WELCOME");
        assert_eq!(reply_name(RPL_MOTDSTART), "RPL_MOTDSTART");
        assert_eq!(reply_name(ERR_NICKNAMEINUSE), "ERR_NICKNAMEINUSE");
        assert_eq!(reply_name(ERR_CHANOPRIVSNEEDED), "ERR_CHANOPRIVSNEEDED");
        assert_eq!(reply_name(RPL_NAMREPLY), "RPL_NAMREPLY");
        assert_eq!(reply_name(RPL_ENDOFNAMES), "RPL_ENDOFNAMES");
    }

    #[test]
    fn reply_name_unknown_code() {
        assert_eq!(reply_name(0), "UNKNOWN");
        assert_eq!(reply_name(999), "UNKNOWN");
        assert_eq!(reply_name(9999), "UNKNOWN");
    }

    #[test]
    fn reply_name_covers_all_constants() {
        // Verify that every constant defined above has a mapping.
        let codes = [
            RPL_WELCOME,
            RPL_YOURHOST,
            RPL_CREATED,
            RPL_MYINFO,
            RPL_LUSERCLIENT,
            RPL_LUSEROP,
            RPL_LUSERUNKNOWN,
            RPL_LUSERCHANNELS,
            RPL_LUSERME,
            RPL_AWAY,
            RPL_ISON,
            RPL_UNAWAY,
            RPL_NOWAWAY,
            RPL_WHOISUSER,
            RPL_WHOISSERVER,
            RPL_WHOISOPERATOR,
            RPL_ENDOFWHO,
            RPL_ENDOFWHOIS,
            RPL_WHOISCHANNELS,
            RPL_WHOISSPECIAL,
            RPL_LIST,
            RPL_LISTEND,
            RPL_NOTOPIC,
            RPL_TOPIC,
            RPL_TOPICWHOTIME,
            RPL_CHANNELMODEIS,
            RPL_INVITING,
            RPL_WHOREPLY,
            RPL_NAMREPLY,
            RPL_ENDOFNAMES,
            RPL_MOTD,
            RPL_MOTDSTART,
            RPL_ENDOFMOTD,
            ERR_NOSUCHNICK,
            ERR_NOSUCHCHANNEL,
            ERR_CANNOTSENDTOCHAN,
            ERR_TOOMANYCHANNELS,
            ERR_UNKNOWNCOMMAND,
            ERR_NONICKNAMEGIVEN,
            ERR_ERRONEUSNICKNAME,
            ERR_NICKNAMEINUSE,
            ERR_USERNOTINCHANNEL,
            ERR_NOTONCHANNEL,
            ERR_USERONCHANNEL,
            ERR_NOTREGISTERED,
            ERR_NEEDMOREPARAMS,
            ERR_ALREADYREGISTERED,
            ERR_CHANNELISFULL,
            ERR_INVITEONLYCHAN,
            ERR_BANNEDFROMCHAN,
            ERR_BADCHANNELKEY,
            ERR_CHANOPRIVSNEEDED,
        ];
        for code in codes {
            assert_ne!(
                reply_name(code),
                "UNKNOWN",
                "reply_name({code}) should not be UNKNOWN"
            );
        }
    }
}
