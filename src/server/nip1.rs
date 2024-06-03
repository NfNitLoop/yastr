use serde::Deserialize;
use serde_json::Value;



/// Parse an event from text.
/// 
/// Handles some cases that the upstream library doesn't.
pub(crate) fn parse_client_message(text: &str) -> ParsedEvent {
    return match serde_json::from_str(&text) {
        Ok(m) => ParsedEvent::Message(m),
        Err(serde_error) => {
            if let Some(subscription_id) = parse_subscription_id(&text) {
                let category = serde_error.classify();
                return ParsedEvent::InvalidRequest{
                    subscription_id,
                    error_message: format!("{category:?} error deserializing REQ event: {serde_error}")
                };
            }
            ParsedEvent::Err { message: format!("Message from client was invalid: {serde_error}")}
        }
    };
}

fn parse_subscription_id(text: &str) -> Option<String> {
    let Ok(json) = serde_json::from_str::<Value>(text) else { return None };
    let Some(array) = json.as_array() else { return None };
    let [Value::String(message_type), Value::String(subscription_id), ..] = array.as_slice() else { return None };
    if message_type != "REQ" { return None; }
    return Some(subscription_id.clone())
}

#[derive(Deserialize, Debug)]
struct PartialRequest {

}

pub (crate) enum ParsedEvent {
    Message(nostr::ClientMessage),
    Err{ message: String },
    InvalidRequest { subscription_id: String, error_message: String }
}