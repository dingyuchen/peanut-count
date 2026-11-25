//! Client for OpenAI-compatible chat completion endpoints, such as LM Studio,
//! llama.cpp, or a hosted API, that return JSON constrained by a schema. It
//! reads the fields of a transaction out of an email's text.

use std::{future::Future, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Largest accepted reply, including any reasoning the model returns.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// Reads structured fields from text. Implemented by [`Llm`] and by test doubles.
pub trait Extract {
    /// Returns a JSON value matching `schema`, read from `text` as `system` instructs.
    fn extract(
        &self,
        system: &str,
        text: &str,
        schema: &Value,
    ) -> impl Future<Output = Result<Value>> + Send;
}

#[derive(Clone)]
pub struct Llm {
    http: reqwest::Client,
    url: String,
    model: String,
    token: Option<String>,
}

impl Llm {
    /// `url` is the full endpoint, for example
    /// `http://127.0.0.1:1234/v1/chat/completions`.
    pub fn new(url: String, model: String, token: Option<String>) -> Result<Self> {
        let parsed = url::Url::parse(&url).context("invalid extraction model URL")?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "extraction model URL must use http or https"
        );
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // A local model may reason at length before it answers.
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            url,
            model,
            token,
        })
    }
}

impl Extract for Llm {
    async fn extract(&self, system: &str, text: &str, schema: &Value) -> Result<Value> {
        let mut request = self.http.post(&self.url).json(&json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": text },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": "transaction", "strict": true, "schema": schema },
            },
        }));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let mut response = request.send().await.context("reach extraction model")?;
        let status = response.status();
        // Never log the body: an error page can echo the request, which holds the email.
        if !status.is_success() {
            bail!("extraction model returned HTTP {status}");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("read extraction model reply")?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_REPLY_BYTES {
                bail!("extraction model reply exceeds {MAX_REPLY_BYTES} bytes");
            }
        }
        // A serde_json error can quote a value from the reply, which may echo
        // the email, so report only where decoding failed.
        let quiet = |what: &str, error: serde_json::Error| {
            anyhow::anyhow!(
                "extraction model {what} is not valid JSON ({:?} error at line {} column {})",
                error.classify(),
                error.line(),
                error.column()
            )
        };
        let reply: Value = serde_json::from_slice(&body).map_err(|error| quiet("reply", error))?;
        // With a reasoning model, LM Studio can return the schema-constrained
        // answer as reasoning and leave the content empty, so fall back to it.
        let message = &reply["choices"][0]["message"];
        let content = [&message["content"], &message["reasoning_content"]]
            .into_iter()
            .filter_map(Value::as_str)
            .find(|text| !text.trim().is_empty())
            .context("extraction model reply has no answer")?;
        serde_json::from_str(content).map_err(|error| quiet("answer", error))
    }
}
