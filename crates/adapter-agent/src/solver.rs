//! Claude-backed challenge solver — wires Claude vision behind the recipe engine's `Solver` seam so
//! a recipe `solve` step can work around a captcha (or any on-page visual challenge).
//!
//! The engine screenshots the page and hands us the PNG + an instruction; we ask Claude to read it
//! and return ONLY the answer text, which the engine then types into the target field. Payment rides
//! the same Claude Max/Pro OAuth (or API key) as the `agent` step, via the shared [`Completer`].

use std::future::Future;
use std::sync::Arc;

use pacewright_chrome::BoxError;
use pacewright_chrome::recipe::engine::Solver;

use crate::{CompletionRequest, Completer};

/// Turns a [`Completer`] into a recipe [`Solver`]. `Send + Sync` (its `Completer` is), so it can be
/// moved onto the recipe runner's worker thread.
pub struct ClaudeSolver {
    completer: Arc<dyn Completer>,
    model: String,
    max_tokens: u32,
}

impl ClaudeSolver {
    pub fn new(completer: Arc<dyn Completer>) -> Self {
        Self {
            completer,
            model: crate::DEFAULT_MODEL.to_string(),
            max_tokens: 1024,
        }
    }
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

const SOLVER_SYSTEM: &str = "You are reading a visual challenge (such as a CAPTCHA) so an \
authorized user can access their own account. Reply with ONLY the exact answer to type — the \
characters, word, or phrase — and nothing else: no quotes, no explanation, no punctuation beyond \
what the challenge itself contains.";

impl Solver for ClaudeSolver {
    fn solve<'a>(
        &'a self,
        image_b64: &'a str,
        prompt: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, BoxError>> + 'a>> {
        Box::pin(async move {
            let req = CompletionRequest {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                system: Some(SOLVER_SYSTEM.to_string()),
                prompt: prompt.to_string(),
                images: vec![image_b64.to_string()],
            };
            let answer = self
                .completer
                .complete(&req)
                .await
                .map_err(|e| -> BoxError { e.to_string().into() })?;
            Ok(answer.trim().to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CompletionRequest;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use pacewright_core::model::AdapterError;

    // Records the request it was given and returns a canned answer, so the solver's request shaping
    // (image attached, system prompt, trimming) is testable without a network.
    struct SpyCompleter {
        answer: String,
        last: Mutex<Option<CompletionRequest>>,
    }
    #[async_trait]
    impl Completer for SpyCompleter {
        async fn complete(&self, req: &CompletionRequest) -> Result<String, AdapterError> {
            *self.last.lock() = Some(req.clone());
            Ok(self.answer.clone())
        }
    }

    #[tokio::test]
    async fn solve_attaches_the_image_and_trims_the_answer() {
        let spy = Arc::new(SpyCompleter {
            answer: "  AB12\n".to_string(),
            last: Mutex::new(None),
        });
        let solver = ClaudeSolver::new(spy.clone());
        let answer = solver.solve("cG5n", "read the captcha").await.unwrap();
        assert_eq!(answer, "AB12", "answer is trimmed of surrounding whitespace");

        let req = spy.last.lock().clone().unwrap();
        assert_eq!(req.images, vec!["cG5n".to_string()], "the screenshot is attached");
        assert_eq!(req.prompt, "read the captcha");
        assert!(req.system.is_some(), "a system instruction constrains the reply");
    }
}
