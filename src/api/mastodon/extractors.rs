use axum::{
    extract::{FromRequest, Multipart},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

/// Accepts JSON body, multipart/form-data body, or application/x-www-form-urlencoded body.
/// Mirrors Rails' transparent parameter handling.
pub struct FormOrJson<T>(pub T);

impl<T, S> FromRequest<S> for FormOrJson<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("application/json") {
            Json::<T>::from_request(req, state)
                .await
                .map(|Json(v)| FormOrJson(v))
                .map_err(IntoResponse::into_response)
        } else if content_type.contains("multipart/form-data") {
            let mut multipart = Multipart::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            let mut pairs: Vec<(String, String)> = Vec::new();
            while let Some(field) = multipart
                .next_field()
                .await
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())?
            {
                let name = field.name().unwrap_or("").to_string();
                let value = field.text().await.map_err(|e| {
                    (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response()
                })?;
                pairs.push((name, value));
            }
            let encoded = serde_urlencoded::to_string(&pairs)
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())?;
            serde_urlencoded::from_str::<T>(&encoded)
                .map(FormOrJson)
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())
        } else {
            let bytes = axum::body::Bytes::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            serde_urlencoded::from_bytes::<T>(&bytes)
                .map(FormOrJson)
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())
        }
    }
}

/// Accepts a JSON body OR URL query parameters (using serde_qs for bracket-notation
/// arrays like `keys[0]=...`). Used for POST endpoints where clients like Nicolium
/// pass params in the query string instead of the body.
pub struct QueryOrJson<T>(pub T);

impl<T, S> FromRequest<S> for QueryOrJson<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("application/json") {
            let (parts, body) = req.into_parts();
            let req = axum::extract::Request::from_parts(parts, body);
            Json::<T>::from_request(req, state)
                .await
                .map(|Json(v)| QueryOrJson(v))
                .map_err(IntoResponse::into_response)
        } else {
            let (parts, _body) = req.into_parts();
            let query = parts.uri.query().unwrap_or("");
            serde_qs::from_str::<T>(query)
                .map(QueryOrJson)
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())
        }
    }
}

/// Rails' `params`: the query string and the body (JSON, form-encoded or
/// multipart), merged with the body winning, then read as `T`. Repeated keys
/// and `key[]` both collect into an array under `key`. Values from a query
/// string or a form arrive as strings; [`FlexId`], [`FlexBool`] and
/// [`FlexIds`] read either form.
pub struct Params<T>(pub T);

fn merge_pairs(
    into: &mut serde_json::Map<String, serde_json::Value>,
    pairs: Vec<(String, String)>,
) {
    use serde_json::Value;
    let mut arrays: std::collections::HashMap<String, Vec<Value>> = Default::default();
    for (key, value) in pairs {
        if let Some(base) = key.strip_suffix("[]") {
            arrays
                .entry(base.to_owned())
                .or_default()
                .push(Value::String(value));
        } else {
            into.insert(key, Value::String(value));
        }
    }
    for (key, values) in arrays {
        into.insert(key, Value::Array(values));
    }
}

impl<T, S> FromRequest<S> for Params<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        use serde_json::Value;
        let unprocessable = |e: String| (StatusCode::UNPROCESSABLE_ENTITY, e).into_response();
        let mut merged = serde_json::Map::new();
        let query = req.uri().query().unwrap_or("").to_owned();
        merge_pairs(
            &mut merged,
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect(),
        );

        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.contains("application/json") {
            let bytes = axum::body::Bytes::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            if !bytes.is_empty() {
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(Value::Object(body)) => merged.extend(body),
                    Ok(_) => {}
                    Err(e) => return Err((StatusCode::BAD_REQUEST, e.to_string()).into_response()),
                }
            }
        } else if content_type.contains("multipart/form-data") {
            let mut multipart = Multipart::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            let mut pairs = vec![];
            while let Some(field) = multipart
                .next_field()
                .await
                .map_err(|e| unprocessable(e.to_string()))?
            {
                let name = field.name().unwrap_or("").to_string();
                let value = field
                    .text()
                    .await
                    .map_err(|e| unprocessable(e.to_string()))?;
                pairs.push((name, value));
            }
            merge_pairs(&mut merged, pairs);
        } else {
            let bytes = axum::body::Bytes::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            merge_pairs(
                &mut merged,
                url::form_urlencoded::parse(&bytes).into_owned().collect(),
            );
        }
        serde_json::from_value::<T>(Value::Object(merged))
            .map(Params)
            .map_err(|e| unprocessable(e.to_string()))
    }
}

/// An id given as a number or a string, as Rails takes either.
#[derive(Debug, Clone, Copy)]
pub struct FlexId(pub i64);

impl<'de> serde::Deserialize<'de> for FlexId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(FlexId)
                .ok_or_else(|| serde::de::Error::custom("invalid id")),
            serde_json::Value::String(s) => s
                .trim()
                .parse()
                .map(FlexId)
                .map_err(|_| serde::de::Error::custom("invalid id")),
            _ => Err(serde::de::Error::custom("invalid id")),
        }
    }
}

/// Ids given as an array of numbers or strings; ones that do not parse are
/// dropped, as Rails' `find` would not match them either.
#[derive(Debug, Clone, Default)]
pub struct FlexIds(pub Vec<i64>);

impl<'de> serde::Deserialize<'de> for FlexIds {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let values = match serde_json::Value::deserialize(d)? {
            serde_json::Value::Array(values) => values,
            serde_json::Value::Null => vec![],
            other => vec![other],
        };
        Ok(FlexIds(
            values
                .into_iter()
                .filter_map(|v| match v {
                    serde_json::Value::Number(n) => n.as_i64(),
                    serde_json::Value::String(s) => s.trim().parse().ok(),
                    _ => None,
                })
                .collect(),
        ))
    }
}

/// `ActiveModel::Type::Boolean#cast`: everything but the false values
/// (`false`, `0`, `"0"`, `"f"`, `"false"`, `"off"`, and their capitalisations)
/// is true; an empty string is nil.
#[derive(Debug, Clone, Copy)]
pub struct FlexBool(pub bool);

impl<'de> serde::Deserialize<'de> for FlexBool {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(FlexBool(match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(b) => b,
            serde_json::Value::Number(n) => n.as_f64() != Some(0.0),
            serde_json::Value::String(s) => {
                !matches!(s.to_lowercase().as_str(), "false" | "0" | "f" | "off" | "")
            }
            serde_json::Value::Null => false,
            _ => true,
        }))
    }
}

/// One parameter of a request body: text, or an uploaded file.
#[derive(Debug, Clone)]
pub enum Part {
    Text(String),
    File {
        content_type: String,
        data: Vec<u8>,
        /// The name the file was uploaded under, `original_filename`.
        file_name: Option<String>,
    },
}

impl Part {
    /// The text of a text part; a file reads as empty.
    pub fn text(&self) -> String {
        match self {
            Part::Text(t) => t.clone(),
            Part::File { .. } => String::new(),
        }
    }

    /// The content type and bytes of a file part; text is no file.
    pub fn file(&self) -> (String, Vec<u8>) {
        match self {
            Part::File {
                content_type, data, ..
            } => (content_type.clone(), data.clone()),
            Part::Text(_) => ("application/octet-stream".into(), vec![]),
        }
    }
}

/// A request body's parameters under Rails' bracketed names
/// (`source[privacy]`, `fields_attributes[0][name]`, `attribution_domains[]`),
/// from multipart, form-encoded or JSON, as Rails reads all three into the
/// same `params`.
pub struct Parts(pub Vec<(String, Part)>);

fn flatten_json(prefix: &str, value: &serde_json::Value, out: &mut Vec<(String, Part)>) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let name = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}[{k}]")
                };
                flatten_json(&name, v, out);
            }
        }
        Value::Array(items) => {
            // Arrays of objects are indexed (`fields_attributes[0][name]`);
            // arrays of scalars repeat `name[]`.
            for (i, v) in items.iter().enumerate() {
                if v.is_object() {
                    flatten_json(&format!("{prefix}[{i}]"), v, out);
                } else {
                    flatten_json(&format!("{prefix}[]"), v, out);
                }
            }
        }
        Value::Null => out.push((prefix.to_owned(), Part::Text(String::new()))),
        Value::String(s) => out.push((prefix.to_owned(), Part::Text(s.clone()))),
        other => out.push((prefix.to_owned(), Part::Text(other.to_string()))),
    }
}

impl<S> FromRequest<S> for Parts
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let unprocessable = |e: String| (StatusCode::UNPROCESSABLE_ENTITY, e).into_response();
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut parts = vec![];
        if content_type.contains("multipart/form-data") {
            let mut multipart = Multipart::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            while let Some(field) = multipart
                .next_field()
                .await
                .map_err(|e| unprocessable(e.to_string()))?
            {
                let name = field.name().unwrap_or("").to_string();
                let file_name = field.file_name().map(str::to_owned);
                let is_file = file_name.is_some();
                let ct = field.content_type().map(str::to_owned);
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| unprocessable(e.to_string()))?;
                if is_file {
                    parts.push((
                        name,
                        Part::File {
                            content_type: ct.unwrap_or_else(|| "application/octet-stream".into()),
                            data: data.to_vec(),
                            file_name,
                        },
                    ));
                } else {
                    parts.push((
                        name,
                        Part::Text(String::from_utf8_lossy(&data).into_owned()),
                    ));
                }
            }
        } else {
            let bytes = axum::body::Bytes::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            if content_type.contains("application/json") {
                if !bytes.is_empty() {
                    let value: serde_json::Value = serde_json::from_slice(&bytes)
                        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
                    flatten_json("", &value, &mut parts);
                }
            } else {
                parts.extend(
                    url::form_urlencoded::parse(&bytes)
                        .into_owned()
                        .map(|(k, v)| (k, Part::Text(v))),
                );
            }
        }
        Ok(Parts(parts))
    }
}
