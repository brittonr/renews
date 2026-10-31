//! Response constants module.
//!
//! Contains all NNTP response codes and messages used throughout the server.

// Basic response codes
pub const RESP_CRLF: &str = "\r\n";
pub const RESP_DOT_CRLF: &str = ".\r\n";

// Connection and status responses
pub const RESP_200_READY: &str = "200 NNTP Service Ready\r\n";
pub const RESP_201_READY_NO_POST: &str = "201 NNTP Service Ready - no posting allowed\r\n";
pub const RESP_200_POSTING_ALLOWED: &str = "200 Posting allowed\r\n";
pub const RESP_201_POSTING_PROHIBITED: &str = "201 Posting prohibited\r\n";
pub const RESP_203_STREAMING: &str = "203 Streaming permitted\r\n";
pub const RESP_205_CLOSING: &str = "205 closing connection\r\n";

// Article responses
pub const RESP_220_ARTICLE: &str = "220";
pub const RESP_221_HEAD: &str = "221";
pub const RESP_222_BODY: &str = "222";
pub const RESP_223_STAT: &str = "223";
pub const RESP_224_OVERVIEW: &str = "224 Overview information follows\r\n";
pub const RESP_225_HEADERS: &str = "225 Headers follow\r\n";

// Group and list responses
pub const RESP_211_GROUP: &str = "211";
pub const RESP_211_LISTGROUP: &str = "211 article numbers follow\r\n";
pub const RESP_215_LIST_FOLLOWS: &str = "215 list of newsgroups follows\r\n";
pub const RESP_215_DESCRIPTIONS: &str = "215 descriptions follow\r\n";
pub const RESP_215_INFO_FOLLOWS: &str = "215 information follows\r\n";
pub const RESP_215_OVERVIEW_FMT: &str = "215 Order of fields in overview database.\r\n";
pub const RESP_215_METADATA: &str = "215 metadata items supported:\r\n";
pub const RESP_221_HEADER_FOLLOWS: &str = "221 Header follows\r\n";
pub const RESP_230_NEWNEWS: &str = "230 list of new articles follows\r\n";
pub const RESP_231_NEWGROUPS: &str = "231 list of new newsgroups follows\r\n";

// Posting responses
pub const RESP_235_TRANSFER_OK: &str = "235 Article transferred OK\r\n";
pub const RESP_238_CHECK_OK: &str = "238";
pub const RESP_239_TAKETHIS_OK: &str = "239";
pub const RESP_240_ARTICLE_RECEIVED: &str = "240 article received\r\n";

// Authentication responses
pub const RESP_281_AUTH_OK: &str = "281 authentication accepted\r\n";
pub const RESP_290_PASSWORD_OK: &str = "290 Password for {user} accepted\r\n";

// Error responses
pub const RESP_340_SEND_ARTICLE: &str =
    "340 send article to be posted. End with <CR-LF>.<CR-LF>\r\n";
pub const RESP_335_SEND_IT: &str = "335 Send it; end with <CR-LF>.<CR-LF>\r\n";
pub const RESP_381_PASSWORD_REQ: &str = "381 password required\r\n";

// 4xx error responses
pub const RESP_403_BANDWIDTH_EXCEEDED: &str = "403 bandwidth limit exceeded\r\n";
pub const RESP_411_NO_SUCH_GROUP: &str = "411 no such newsgroup\r\n";
pub const RESP_412_NO_GROUP: &str = "412 no newsgroup selected\r\n";
pub const RESP_420_NO_CURRENT: &str = "420 no current article selected\r\n";
pub const RESP_421_NO_NEXT: &str = "421 no next article\r\n";
pub const RESP_422_NO_PREV: &str = "422 no previous article\r\n";
pub const RESP_423_RANGE_EMPTY: &str = "423 no articles in that range\r\n";
pub const RESP_423_NO_ARTICLE_NUM: &str = "423 no such article number in this group\r\n";
pub const RESP_430_NO_ARTICLE: &str = "430 no such article\r\n";
pub const RESP_435_NOT_WANTED: &str = "435 article not wanted\r\n";
pub const RESP_437_REJECTED: &str = "437 article rejected\r\n";
pub const RESP_438_CHECK_REJECT: &str = "438";
pub const RESP_439_TAKETHIS_REJECT: &str = "439";
pub const RESP_440_POST_PROHIBITED: &str = "440 posting not allowed\r\n";
pub const RESP_441_POSTING_FAILED: &str = "441 posting failed\r\n";
pub const RESP_441_RATE_LIMIT: &str = "441 posting rate limit exceeded\r\n";
pub const RESP_480_AUTH_REQUIRED: &str = "480 authentication required\r\n";
pub const RESP_481_AUTH_REJECTED: &str = "481 Authentication rejected\r\n";
pub const RESP_481_CONN_LIMIT: &str = "481 connection limit exceeded\r\n";
pub const RESP_483_SECURE_REQ: &str = "483 Secure connection required\r\n";

// 5xx error responses
pub const RESP_500_SYNTAX: &str = "500 Syntax error\r\n";
pub const RESP_500_UNKNOWN_CMD: &str = "500 command not recognized\r\n";
pub const RESP_501_SYNTAX: &str = "501 Syntax error\r\n";
pub const RESP_501_INVALID_ID: &str = "501 invalid id\r\n";
pub const RESP_501_INVALID_ARG: &str = "501 invalid argument\r\n";
pub const RESP_501_INVALID_DATE: &str = "501 invalid date\r\n";
pub const RESP_501_MSGID_REQUIRED: &str = "501 message-id required\r\n";
pub const RESP_501_NOT_ENOUGH: &str = "501 not enough arguments\r\n";
pub const RESP_501_UNKNOWN_KEYWORD: &str = "501 unknown keyword\r\n";
pub const RESP_501_UNKNOWN_MODE: &str = "501 unknown mode\r\n";
pub const RESP_501_MISSING_MODE: &str = "501 missing mode\r\n";
pub const RESP_503_NOT_SUPPORTED: &str = "503 feature not supported\r\n";

// Capability responses
pub const RESP_101_CAPABILITIES: &str = "101 Capability list follows\r\n";
pub const RESP_100_HELP_FOLLOWS: &str = "100 help text follows\r\n";

// Capability list items
pub const RESP_CAP_VERSION: &str = "VERSION 2\r\n";
pub const RESP_CAP_IMPLEMENTATION: &str =
    concat!("IMPLEMENTATION Renews ", env!("CARGO_PKG_VERSION"), "\r\n");
pub const RESP_CAP_READER: &str = "READER\r\n";
pub const RESP_CAP_IHAVE: &str = "IHAVE\r\n";
pub const RESP_CAP_POST: &str = "POST\r\n";
pub const RESP_CAP_NEWNEWS: &str = "NEWNEWS\r\n";
pub const RESP_CAP_HDR: &str = "HDR\r\n";
pub const RESP_CAP_OVER: &str = "OVER MSGID\r\n";
pub const RESP_CAP_LIST: &str = "LIST ACTIVE NEWSGROUPS ACTIVE.TIMES OVERVIEW.FMT HEADERS\r\n";
pub const RESP_CAP_AUTHINFO: &str = "AUTHINFO USER\r\n";
pub const RESP_CAP_STREAMING: &str = "STREAMING\r\n";

// Help text
pub const RESP_HELP_TEXT: &str = concat!(
    "CAPABILITIES\r\n",
    "MODE READER\r\n",
    "MODE STREAM\r\n",
    "GROUP\r\n",
    "LIST\r\n",
    "LISTGROUP\r\n",
    "ARTICLE\r\n",
    "HEAD\r\n",
    "BODY\r\n",
    "STAT\r\n",
    "HDR\r\n",
    "OVER\r\n",
    "NEXT\r\n",
    "LAST\r\n",
    "NEWGROUPS\r\n",
    "NEWNEWS\r\n",
    "IHAVE\r\n",
    "CHECK\r\n",
    "TAKETHIS\r\n",
    "POST\r\n",
    "DATE\r\n",
    "HELP\r\n",
    "QUIT\r\n"
);

// Overview format
pub const RESP_SUBJECT: &str = "Subject:\r\n";
pub const RESP_FROM: &str = "From:\r\n";
pub const RESP_DATE: &str = "Date:\r\n";
pub const RESP_MESSAGE_ID: &str = "Message-ID:\r\n";
pub const RESP_REFERENCES: &str = "References:\r\n";
pub const RESP_BYTES: &str = ":bytes\r\n";
pub const RESP_LINES: &str = ":lines\r\n";
pub const RESP_COLON: &str = ":\r\n";

/// Format a streaming protocol response (CHECK/TAKETHIS).
///
/// Used for responses that include a message-id, such as:
/// - 238/438 for CHECK
/// - 239/439 for TAKETHIS
#[inline]
pub fn streaming_response(code: u16, message_id: &str) -> String {
    format!("{code} {message_id}\r\n")
}
