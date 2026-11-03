//! Using an AI model. The host holds the API key and makes the request, so an agent never sees it.

use crate::Error;
use crate::bindings::habitat::host::agent_home as api;
use crate::bindings::habitat::host::types::{AccessTarget, ModelMessage, ModelRequest, ModelRole};

/// One message to a model.
#[derive(Debug, Clone)]
pub struct Message(ModelMessage);

impl Message {
    /// Tells the model how to behave.
    pub fn system(text: &str) -> Self {
        Self(ModelMessage {
            role: ModelRole::System,
            text: text.to_string(),
        })
    }

    pub fn user(text: &str) -> Self {
        Self(ModelMessage {
            role: ModelRole::User,
            text: text.to_string(),
        })
    }

    pub fn assistant(text: &str) -> Self {
        Self(ModelMessage {
            role: ModelRole::Assistant,
            text: text.to_string(),
        })
    }
}

/// Asks the user to let this agent use a model. The model has to be listed in the manifest. The
/// reason is shown to the user, marked as this agent's own claim.
pub fn ask_access(model: &str, reason: &str) -> Result<(), Error> {
    api::request_access(&AccessTarget::Model(model.to_string()), reason)?;
    Ok(())
}

/// Asks a model something, and waits for the whole answer. To show an answer as it arrives, use
/// `bindings` and read its stream instead.
pub fn complete(model: &str, messages: &[Message], max_tokens: u32) -> Result<String, Error> {
    let answer = api::model_complete(&ModelRequest {
        model: model.to_string(),
        messages: messages.iter().map(|message| message.0.clone()).collect(),
        max_tokens,
    })?;
    let mut text = String::new();
    while let Some(piece) = answer.next()? {
        text.push_str(&piece);
    }
    Ok(text)
}
