//! Client for decision models that speak TypeSafe Jev's `/v1/systemone` wire
//! format: hosted Jev, OpenRouter, LM Studio, and llama.cpp all serve it.

use std::{collections::BTreeMap, future::Future, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// Largest accepted reply. Answers to a few hundred options are a few kilobytes.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// One typed answer. Only the shapes this server asks for are decoded.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
    },
    Refusal,
    #[serde(other)]
    Unsupported,
}

pub type Answers = BTreeMap<String, Answer>;

/// Answers named questions about a state. Implemented by [`Jev`] and by test doubles.
pub trait Decide {
    fn decide(
        &self,
        state: Value,
        questions: Value,
    ) -> impl Future<Output = Result<Answers>> + Send;
}

#[derive(Clone)]
pub struct Jev {
    http: reqwest::Client,
    url: String,
    model: String,
    token: Option<String>,
}

impl Jev {
    /// `url` is the full endpoint, for example `http://127.0.0.1:1234/v1/systemone`.
    pub fn new(url: String, model: String, token: Option<String>) -> Result<Self> {
        let parsed = url::Url::parse(&url).context("invalid decision model URL")?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "decision model URL must use http or https"
        );
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Local CPU models can take a while on a long email.
            .timeout(Duration::from_secs(120))
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

impl Decide for Jev {
    async fn decide(&self, state: Value, questions: Value) -> Result<Answers> {
        #[derive(Deserialize)]
        struct Reply {
            answers: Answers,
        }
        let mut request = self.http.post(&self.url).json(&json!({
            "model": self.model, "state": state, "questions": questions,
        }));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let mut response = request.send().await.context("reach decision model")?;
        let status = response.status();
        // Never log the body: an error page can echo the request, which holds the email.
        if !status.is_success() {
            bail!("decision model returned HTTP {status}");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("read decision model answers")?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_REPLY_BYTES {
                bail!("decision model answers exceed {MAX_REPLY_BYTES} bytes");
            }
        }
        // A serde_json error can quote a value from the reply, which may echo the
        // email, so report only where decoding failed.
        let reply = serde_json::from_slice::<Reply>(&body).map_err(|error| {
            anyhow::anyhow!(
                "decision model answers are not valid ({:?} error at line {} column {})",
                error.classify(),
                error.line(),
                error.column()
            )
        })?;
        Ok(reply.answers)
    }
}
