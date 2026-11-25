//! Client for Clairic Flash, Cloudflare's typed extraction model. It reads the
//! fields of a transaction out of an email's text, then chooses the account
//! and category from options it is given. Both hosts speak
//! System One: Extract (`state` and typed `fields`):
//!
//! - [`Host::Local`]: the model's own MLX server, `server.py` in
//!   `mlx-community/clairic-flash-4bit`, on this machine.
//! - [`Host::Cloudflare`]: Workers AI, which receives the text of every email.

use std::{collections::BTreeMap, future::Future, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDate;
use serde_json::{Map, Value, json};

/// Largest accepted reply.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// The model name both hosts accept, and Workers AI requires.
const MODEL: &str = "clairic-flash";

/// Default endpoint of the local server.
pub const LOCAL_URL: &str = "http://127.0.0.1:8093/v1/systemone";

/// What a field holds.
#[derive(Clone, Copy, Debug)]
pub enum Kind<'a> {
    /// True or false.
    Boolean,
    /// One of a fixed set of words.
    Choice(&'a [&'a str]),
    /// A number, answered as a JSON number.
    Number,
    /// Text copied from the email.
    Text,
    /// A short code the email may write differently, such as SGD for "S$".
    Code,
    /// A date, answered as YYYY-MM-DD.
    Date,
}

/// One value to read.
#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    pub name: &'a str,
    pub kind: Kind<'a>,
    pub instructions: &'a str,
}

/// What to read from one text.
pub struct Request<'a> {
    pub text: &'a str,
    /// The date the text was written, for dates that omit the year.
    pub reference_date: Option<NaiveDate>,
    pub fields: &'a [Field<'a>],
}

/// The model's answer for one field.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The value, or null where the text gives none.
    pub value: Value,
    /// The model's probability for the value, where it reports one.
    pub confidence: Option<f64>,
}

/// Answers by field name.
pub type Answers = BTreeMap<String, Answer>;

/// Reads typed fields from text. Implemented by [`Clairic`] and by test doubles.
pub trait Extract {
    /// Returns an answer for each field.
    fn extract(&self, request: &Request<'_>) -> impl Future<Output = Result<Answers>> + Send;
}

/// Where Clairic runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Host {
    /// The model's own server on this machine, or one the operator runs.
    Local,
    /// Cloudflare Workers AI.
    Cloudflare,
}

#[derive(Clone)]
pub struct Clairic {
    http: reqwest::Client,
    url: String,
    token: Option<String>,
}

impl Clairic {
    /// A local server at `url`, such as [`LOCAL_URL`]. `token` is the
    /// server's `CLAIRIC_API_KEY`, if it sets one.
    pub fn local(url: String, token: Option<String>) -> Result<Self> {
        let parsed = url::Url::parse(&url).context("invalid Clairic URL")?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "Clairic URL must use http or https"
        );
        // A local model may take a minute on a busy machine.
        Self::new(url, token, Duration::from_secs(300))
    }

    /// Workers AI in the Cloudflare account `account_id`, with an API token
    /// allowed to run Workers AI models.
    pub fn cloudflare(account_id: &str, token: String) -> Result<Self> {
        ensure!(
            !account_id.is_empty()
                && account_id.len() <= 64
                && account_id.bytes().all(|b| b.is_ascii_alphanumeric()),
            "invalid Cloudflare account ID"
        );
        ensure!(!token.trim().is_empty(), "Workers AI needs an API token");
        let url = format!(
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run/@cf/cloudflare/{MODEL}"
        );
        Self::new(url, Some(token), Duration::from_secs(120))
    }

    fn new(url: String, token: Option<String>, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, url, token })
    }

    async fn post(&self, body: &Value) -> Result<Value> {
        let mut request = self.http.post(&self.url).json(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let mut response = request.send().await.context("reach Clairic")?;
        let status = response.status();
        // Never log the body: an error can echo the request, which holds the email.
        if !status.is_success() {
            bail!("Clairic returned HTTP {status}");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.context("read Clairic reply")? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_REPLY_BYTES {
                bail!("Clairic reply exceeds {MAX_REPLY_BYTES} bytes");
            }
        }
        // A serde_json error can quote a value from the reply, which may echo
        // the email, so report only where decoding failed.
        serde_json::from_slice(&body).map_err(|error| {
            anyhow::anyhow!(
                "Clairic reply is not valid JSON ({:?} error at line {} column {})",
                error.classify(),
                error.line(),
                error.column()
            )
        })
    }
}

/// The System One: Extract specification of each field.
pub fn specs(fields: &[Field<'_>]) -> Map<String, Value> {
    fields
        .iter()
        .map(|field| {
            let mut spec = match field.kind {
                Kind::Boolean => json!({ "type": "boolean" }),
                Kind::Choice(words) => json!({ "type": "enum", "enum": words }),
                Kind::Number => json!({ "type": "number" }),
                Kind::Text => json!({ "type": "text" }),
                // Not copied: "SGD" may be written "S$".
                Kind::Code => json!({ "type": "text", "knowledge": true }),
                Kind::Date => json!({ "type": "date" }),
            };
            spec["instructions"] = field.instructions.into();
            (field.name.to_owned(), spec)
        })
        .collect()
}

/// The answer for each field in a reply, null where the model found none.
fn values(reply: &Value, fields: &[Field<'_>]) -> Result<Answers> {
    // Cloudflare's REST API wraps a model's output in `result`.
    let reply = match reply.get("result") {
        Some(result) if result.get("answers").is_some() => result,
        _ => reply,
    };
    let answers = reply
        .get("answers")
        .and_then(Value::as_object)
        .context("Clairic reply has no answers")?;
    fields
        .iter()
        .map(|field| {
            let answer = answers
                .get(field.name)
                .with_context(|| format!("Clairic did not answer `{}`", field.name))?;
            // A value that failed its type's grammar is no value.
            let value = if answer.get("valid") == Some(&Value::Bool(false)) {
                Value::Null
            } else {
                answer.get("value").cloned().unwrap_or(Value::Null)
            };
            let confidence = answer.get("confidence").and_then(Value::as_f64);
            Ok((field.name.to_owned(), Answer { value, confidence }))
        })
        .collect()
}

impl Extract for Clairic {
    async fn extract(&self, request: &Request<'_>) -> Result<Answers> {
        let mut body = json!({
            "model": MODEL,
            "state": request.text,
            "fields": specs(request.fields),
        });
        if let Some(date) = request.reference_date {
            body["reference_date"] = date.to_string().into();
        }
        values(&self.post(&body).await?, request.fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: &[Field<'static>] = &[
        Field {
            name: "amount",
            kind: Kind::Number,
            instructions: "The amount.",
        },
        Field {
            name: "currency",
            kind: Kind::Code,
            instructions: "The currency.",
        },
    ];

    #[test]
    fn fields_become_typed_specs() {
        let specs = specs(FIELDS);
        assert_eq!(
            specs["amount"],
            json!({ "type": "number", "instructions": "The amount." })
        );
        assert_eq!(specs["currency"]["knowledge"], true);
    }

    #[test]
    fn reads_bare_and_wrapped_replies() {
        let answers = json!({ "answers": {
            "amount": { "type": "number", "value": 12.5, "confidence": 0.99 },
            // A value that failed its grammar is no value.
            "currency": { "type": "text", "value": "S", "valid": false },
        }});
        let expected = Answers::from([
            (
                "amount".into(),
                Answer {
                    value: json!(12.5),
                    confidence: Some(0.99),
                },
            ),
            (
                "currency".into(),
                Answer {
                    value: Value::Null,
                    confidence: None,
                },
            ),
        ]);
        assert_eq!(values(&answers, FIELDS).unwrap(), expected);
        let wrapped = json!({ "success": true, "errors": [], "result": answers });
        assert_eq!(values(&wrapped, FIELDS).unwrap(), expected);
        assert!(values(&json!({ "answers": {} }), FIELDS).is_err());
    }

    #[test]
    fn workers_ai_needs_an_account_and_a_token() {
        assert!(Clairic::cloudflare("0123abcd", "token".into()).is_ok());
        assert!(Clairic::cloudflare("../other", "token".into()).is_err());
        assert!(Clairic::cloudflare("0123abcd", " ".into()).is_err());
        assert!(Clairic::local("file:///etc/passwd".into(), None).is_err());
    }
}
