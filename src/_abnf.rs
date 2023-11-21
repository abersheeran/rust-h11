use lazy_static::lazy_static;

pub static OWS: &str = r"[ \t]*";
pub static TOKEN: &str = r"[-!#$%&'*+.^_`|~0-9a-zA-Z]+";
pub static FIELD_NAME: &str = TOKEN;
pub static VCHAR: &str = r"[\x21-\x7e]";
pub static VCHAR_OR_OBS_TEXT: &str = r"[^\x00\s]";
pub static FIELD_VCHAR: &str = VCHAR_OR_OBS_TEXT;

lazy_static! {
    pub static ref FIELD_CONTENT: String = format!(r"{}+(?:[ \t]+{}+)*", FIELD_VCHAR, FIELD_VCHAR);
    pub static ref FIELD_VALUE: String = format!(r"({})?", *FIELD_CONTENT);
    pub static ref HEADER_FIELD: String = format!(
        r"(?P<field_name>{field_name}):{OWS}(?P<field_value>{field_value}){OWS}",
        field_name = FIELD_NAME,
        field_value = *FIELD_VALUE,
        OWS = OWS
    );
    pub static ref METHOD: String = TOKEN.to_string();
    pub static ref REQUEST_TARGET: String = format!("{}+", VCHAR);
    pub static ref HTTP_VERSION: String = r"HTTP/(?P<http_version>[0-9]\.[0-9])".to_string();
    pub static ref REQUEST_LINE: String = format!(
        r"(?P<method>{method}) (?P<target>{request_target}) {http_version}",
        method = *METHOD,
        request_target = *REQUEST_TARGET,
        http_version = *HTTP_VERSION
    );
    pub static ref STATUS_CODE: String = r"[0-9]{3}".to_string();
    pub static ref REASON_PHRASE: String = format!(r"([ \t]|{})*", VCHAR_OR_OBS_TEXT);
    pub static ref STATUS_LINE: String = format!(
        r"{http_version} (?P<status_code>{status_code})(?: (?P<reason>{reason_phrase}))?",
        http_version = *HTTP_VERSION,
        status_code = *STATUS_CODE,
        reason_phrase = *REASON_PHRASE
    );
    pub static ref HEXDIG: String = r"[0-9A-Fa-f]".to_string();
    pub static ref CHUNK_SIZE: String = format!(r"({}){{1,20}}", *HEXDIG);
    pub static ref CHUNK_EXT: String = ";.*".to_string();
    pub static ref CHUNK_HEADER: String = format!(
        r"(?P<chunk_size>{chunk_size})(?P<chunk_ext>{chunk_ext})?{OWS}\r\n",
        chunk_size = *CHUNK_SIZE,
        chunk_ext = *CHUNK_EXT,
        OWS = OWS
    );
}
