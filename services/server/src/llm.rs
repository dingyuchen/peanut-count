//! Client for the model that reads the fields of a transaction out of an
//! email's text. It speaks either System One: Extract (`/v1/systemone` with
//! `state` and typed `fields`, as served by Clairic locally or by Workers AI),
//! or OpenAI-compatible chat completions with a JSON schema (LM Studio,
//! llama.cpp, or a hosted API).

use std::{future::Future, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use serde_json::{Map, Value, json};

/// Largest accepted reply, including any reasoning the model returns.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// What a field holds.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// True or false.
    Boolean,
    /// One of a fixed set of words.
    Choice(&'static [&'static str]),
    /// A number. The answer is a JSON number or the number as written.
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
pub struct Field {
    pub name: &'static str,
    pub kind: Kind,
    pub instructions: &'static str,
}

/// What to read from one text.
pub struct Request<'a> {
    /// What the text is and how to read it, for models that take a prompt.
    pub task: &'a str,
    pub text: &'a str,
    /// The date the text was written, for dates that omit the year.
    pub reference_date: Option<NaiveDate>,
    pub fields: &'a [Field],
}

/// Reads typed fields from text. Implemented by [`Reader`] and by test doubles.
pub trait Extract {
    /// Returns an object with one value, or null, for each field.
    fn extract(&self, request: &Request<'_>) -> impl Future<Output = Result<Value>> + Send;
}

/// The wire format of the extraction endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// System One: Extract, `state` and typed `fields`, as Clairic serves it.
    Systemone,
    /// OpenAI-compatible chat completions with a JSON schema response format.
    Chat,
}

#[derive(Clone)]
pub struct Reader {
    http: reqwest::Client,
    url: String,
    format: Format,
    model: Option<String>,
    token: Option<String>,
}

impl Reader {
    /// `url` is the full endpoint, for example
    /// `http://127.0.0.1:8093/v1/systemone`. A chat endpoint needs `model`.
    pub fn new(
        url: String,
        format: Format,
        model: Option<String>,
        token: Option<String>,
    ) -> Result<Self> {
        let parsed = url::Url::parse(&url).context("invalid extraction model URL")?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "extraction model URL must use http or https"
        );
        anyhow::ensure!(
            format != Format::Chat || model.is_some(),
            "a chat extraction endpoint needs a model ID"
        );
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // A local model may take minutes on a busy machine, or reason at
            // length before it answers.
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            url,
            format,
            model,
            token,
        })
    }

    async fn post(&self, body: &Value) -> Result<Value> {
        let mut request = self.http.post(&self.url).json(body);
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
        serde_json::from_slice(&body).map_err(|error| quiet("reply", &error))
    }

    async fn systemone(&self, request: &Request<'_>) -> Result<Value> {
        let fields: Map<String, Value> = request
            .fields
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
            .collect();
        let mut body = json!({ "state": request.text, "fields": fields });
        if let Some(date) = request.reference_date {
            body["reference_date"] = date.to_string().into();
        }
        if let Some(model) = &self.model {
            body["model"] = model.as_str().into();
        }
        let reply = self.post(&body).await?;
        // Workers AI's REST API wraps the answer in `result`.
        let reply = match reply.get("result") {
            Some(result) if result.get("answers").is_some() => result,
            _ => &reply,
        };
        let answers = reply
            .get("answers")
            .and_then(Value::as_object)
            .context("extraction model reply has no answers")?;
        request
            .fields
            .iter()
            .map(|field| {
                let answer = answers
                    .get(field.name)
                    .with_context(|| format!("extraction model did not answer `{}`", field.name))?;
                // A value that failed its type's grammar is no value.
                let value = if answer.get("valid") == Some(&Value::Bool(false)) {
                    Value::Null
                } else {
                    answer.get("value").cloned().unwrap_or(Value::Null)
                };
                Ok((field.name.to_owned(), value))
            })
            .collect::<Result<Map<_, _>>>()
            .map(Value::Object)
    }

    async fn chat(&self, request: &Request<'_>) -> Result<Value> {
        let mut system = format!("{}\n", request.task);
        let mut properties = Map::new();
        for field in request.fields {
            let (schema, hint) = match field.kind {
                Kind::Boolean => (json!({ "type": "boolean" }), ""),
                Kind::Choice(words) => {
                    let mut allowed: Vec<Value> = words.iter().map(|&word| word.into()).collect();
                    allowed.push(Value::Null);
                    (json!({ "type": ["string", "null"], "enum": allowed }), "")
                }
                Kind::Number => (
                    json!({ "type": ["string", "null"] }),
                    " Write it as the email does, without the currency symbol.",
                ),
                Kind::Text | Kind::Code => (json!({ "type": ["string", "null"] }), ""),
                Kind::Date => (
                    json!({ "type": ["string", "null"] }),
                    " Write it as YYYY-MM-DD. If the email omits the year, use the year of the Sent date, or the previous year if that would put the date after the Sent date.",
                ),
            };
            system.push_str(&format!("- {}: {}{hint}\n", field.name, field.instructions));
            properties.insert(field.name.to_owned(), schema);
        }
        let names: Vec<&str> = request.fields.iter().map(|field| field.name).collect();
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                { "role": "system", "content": system.trim_end() },
                { "role": "user", "content": request.text },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": "transaction", "strict": true, "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": names,
                    "properties": properties,
                }},
            },
        });
        let reply = self.post(&body).await?;
        // With a reasoning model, LM Studio can return the schema-constrained
        // answer as reasoning and leave the content empty, so fall back to it.
        let message = &reply["choices"][0]["message"];
        let content = [&message["content"], &message["reasoning_content"]]
            .into_iter()
            .filter_map(Value::as_str)
            .find(|text| !text.trim().is_empty())
            .context("extraction model reply has no answer")?;
        serde_json::from_str(content).map_err(|error| quiet("answer", &error))
    }
}

/// A serde_json error can quote a value from the reply, which may echo the
/// email, so report only where decoding failed.
fn quiet(what: &str, error: &serde_json::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "extraction model {what} is not valid JSON ({:?} error at line {} column {})",
        error.classify(),
        error.line(),
        error.column()
    )
}

impl Extract for Reader {
    async fn extract(&self, request: &Request<'_>) -> Result<Value> {
        match self.format {
            Format::Systemone => self.systemone(request).await,
            Format::Chat => self.chat(request).await,
        }
    }
}
