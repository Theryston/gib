use super::api::AiMessage;
use super::error::AiBackendError;
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};
use std::fmt::Write as _;

/// Render typed messages with the template embedded in the GGUF whenever
/// llama.cpp exposes one. The fallback is deliberately deterministic and is
/// kept here so callers never need to assemble model-specific prompt strings.
pub(crate) fn render_prompt(
    model: &LlamaModel,
    messages: &[AiMessage],
) -> Result<String, AiBackendError> {
    let chat = messages
        .iter()
        .map(|message| {
            LlamaChatMessage::new(message.role.as_str().to_string(), message.content.clone())
                .map_err(|_| {
                    AiBackendError::InvalidRequest(
                        "prompt messages could not be converted for llama.cpp".to_string(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    if let Ok(template) = model.chat_template(None)
        && let Ok(prompt) = model.apply_chat_template(&template, &chat, true)
    {
        return Ok(prompt);
    }

    Ok(render_fallback(messages))
}

fn render_fallback(messages: &[AiMessage]) -> String {
    let mut prompt = String::new();
    for message in messages {
        prompt.push_str("<|im_start|>");
        prompt.push_str(message.role.as_str());
        prompt.push('\n');
        prompt.push_str(&message.content);
        prompt.push_str("<|im_end|>\n");
    }
    let _ = write!(prompt, "<|im_start|>assistant\n");
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::runtime::api::{AiMessage, AiMessageRole};

    #[test]
    fn fallback_prompt_is_deterministic_and_adds_assistant_turn() {
        let prompt = render_fallback(&[
            AiMessage::new(AiMessageRole::System, "Be concise."),
            AiMessage::new(AiMessageRole::User, "Hello"),
        ]);
        assert_eq!(
            prompt,
            "<|im_start|>system\nBe concise.<|im_end|>\n<|im_start|>user\nHello<|im_end|>\n<|im_start|>assistant\n"
        );
    }
}
