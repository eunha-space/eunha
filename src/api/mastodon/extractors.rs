use axum::{
    extract::{FromRequest, Multipart},
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Rails' `params`, read as `T`: see [`NestedParams`] for how the query
/// string and the body become one hash. Values from a query string or a
/// form arrive as strings; [`FlexId`], [`FlexBool`], [`RubyInt`],
/// [`FlexIds`] and the [`rails`] casts read either form.
pub struct Params<T>(pub T);

impl<T, S> FromRequest<S> for Params<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let NestedParams(value) = NestedParams::from_request(req, state).await?;
        serde_json::from_value::<T>(value)
            .map(Params)
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())
    }
}

/// Casts for fields Rails reads from `params`, where a form or a query
/// string gives every value as a string and a JSON body may give either.
/// Use with `#[serde(default, deserialize_with = "…")]`.
pub mod rails {
    use serde::Deserialize;
    use serde_json::Value;

    /// `ActiveModel::Type::Boolean#cast`: blank is nil, the false values
    /// (`false`, `0`, `"0"`, `"f"`, `"F"`, `"false"`, `"FALSE"`, `"off"`,
    /// `"OFF"`) are false, anything else is true.
    pub fn cast_bool(value: &Value) -> Option<bool> {
        match value {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            Value::Number(n) => Some(n.as_f64() != Some(0.0)),
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(!matches!(
                s.as_str(),
                "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF"
            )),
            _ => Some(true),
        }
    }

    /// `truthy_param?` of a form value: [`cast_bool`] of the string, nil
    /// as false.
    pub fn truthy(value: &str) -> bool {
        cast_bool(&Value::String(value.to_owned())).unwrap_or(false)
    }

    /// An optional boolean as [`cast_bool`] reads it.
    pub fn opt_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        Ok(cast_bool(&Value::deserialize(d)?))
    }

    /// A boolean as `truthy_param?` reads it: nil is false.
    pub fn bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        Ok(cast_bool(&Value::deserialize(d)?).unwrap_or(false))
    }

    /// `ActiveModel::Type::Integer#cast`: a number, or a string's `to_i`;
    /// blank is nil.
    pub fn cast_int(value: &Value) -> Option<i64> {
        match value {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
            Value::Bool(b) => Some(i64::from(*b)),
            Value::String(s) if s.trim().is_empty() => None,
            Value::String(s) => Some(crate::search::ruby_to_i(s)),
            _ => None,
        }
    }

    /// An optional integer as [`cast_int`] reads it.
    pub fn opt_int<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        Ok(cast_int(&Value::deserialize(d)?))
    }

    /// An optional string: a number or a boolean as its text, as Rails
    /// hands a JSON scalar to a string attribute.
    pub fn opt_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Null => None,
            Value::String(s) => Some(s),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
    }

    /// [`opt_string`], blank as nil: what a `.present?` check reads.
    pub fn opt_present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        opt_string(d).map(|s| s.filter(|s| !s.is_empty()))
    }

    /// A string, nil as empty: what a required text field reads.
    pub fn string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        opt_string(d).map(Option::unwrap_or_default)
    }

    /// `accepts_nested_attributes_for`'s collection: an array of hashes,
    /// or a form's hash of them by index (`keywords_attributes[0][keyword]`),
    /// whose values are taken in order; a lone hash with an `id` is one.
    pub fn nested_attributes<'de, D, T>(d: D) -> Result<Option<Vec<T>>, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: serde::de::DeserializeOwned,
    {
        let items = match Value::deserialize(d)? {
            Value::Null => return Ok(None),
            Value::Array(items) => items,
            Value::Object(map) if map.contains_key("id") => vec![Value::Object(map)],
            Value::Object(map) => map.into_iter().map(|(_, v)| v).collect(),
            _ => vec![],
        };
        items
            .into_iter()
            .filter(Value::is_object)
            .map(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))
            .collect::<Result<Vec<T>, _>>()
            .map(Some)
    }

    /// A list of strings, as `param: []` permits: an array, a lone value
    /// as one, nil as none. Non-scalar items are dropped, as `permit`
    /// drops them.
    pub fn strings<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        let items = match Value::deserialize(d)? {
            Value::Array(items) => items,
            Value::Null => vec![],
            // A form's `ids[0]=…&ids[1]=…` is a hash of its values.
            Value::Object(map) => map.into_iter().map(|(_, v)| v).collect(),
            other => vec![other],
        };
        Ok(items
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .collect())
    }

    /// [`opt_strings`] without its blank items, as a form's empty
    /// `media_ids[]` field is no id.
    pub fn opt_present_strings<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<String>>, D::Error> {
        opt_strings(d).map(|v| v.map(|v| v.into_iter().filter(|s| !s.is_empty()).collect()))
    }

    /// [`strings`], `None` when absent.
    pub fn opt_strings<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<String>>, D::Error> {
        let value = Value::deserialize(d)?;
        if value.is_null() {
            return Ok(None);
        }
        strings(value).map(Some).map_err(serde::de::Error::custom)
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

/// A parameter Rails reads with `.to_i`: a number, or a string's leading
/// integer (zero when it has none, an empty string included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RubyInt(pub i64);

impl<'de> serde::Deserialize<'de> for RubyInt {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Number(n) => Ok(RubyInt(
                n.as_i64()
                    .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
                    .unwrap_or_default(),
            )),
            serde_json::Value::String(s) => Ok(RubyInt(crate::search::ruby_to_i(&s))),
            _ => Err(serde::de::Error::custom("invalid integer")),
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

/// `truthy_param?`: [`rails::cast_bool`], nil (absent, null or blank) as
/// false.
#[derive(Debug, Clone, Copy)]
pub struct FlexBool(pub bool);

impl<'de> serde::Deserialize<'de> for FlexBool {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(FlexBool(
            rails::cast_bool(&serde_json::Value::deserialize(d)?).unwrap_or(false),
        ))
    }
}

/// Rails' `params` with Rack's nesting: the body and the query string,
/// merged as `request_parameters.merge(query_parameters)` merges them (the
/// query string's top-level keys winning), where a form's `subscription[keys][auth]`
/// is `{"subscription": {"keys": {"auth": …}}}` and `ids[]` an array, as
/// `Rack::QueryParser#normalize_params` builds them. A JSON body is taken as
/// it is, its booleans and numbers kept; form values are strings. Names that
/// nest in conflicting ways are a 400, as Rack's `ParameterTypeError` is.
pub struct NestedParams(pub serde_json::Value);

/// `Rack::QueryParser#normalize_params`: put `value` into `params` under
/// the bracketed `name`. `Err` when the name conflicts with what is there.
fn normalize_params(
    params: &mut serde_json::Map<String, serde_json::Value>,
    name: &str,
    value: serde_json::Value,
) -> Result<(), ()> {
    use serde_json::{Map, Value};
    // `name =~ %r(\A[\[\]]*([^\[\]]+)\]*)`: the key, and what follows it.
    let trimmed = name.trim_start_matches(['[', ']']);
    let end = trimmed.find(['[', ']']).unwrap_or(trimmed.len());
    let key = &trimmed[..end];
    let after = trimmed[end..].trim_start_matches(']');
    if key.is_empty() {
        return Ok(());
    }
    if after.is_empty() {
        params.insert(key.to_owned(), value);
    } else if after == "[" {
        params.insert(name.to_owned(), value);
    } else if after == "[]" {
        match params
            .entry(key.to_owned())
            .or_insert_with(|| Value::Array(vec![]))
        {
            Value::Array(items) => items.push(value),
            _ => return Err(()),
        }
    } else if let Some(child) = after.strip_prefix("[]") {
        // `[][child]`: an array of hashes, a new one whenever the last
        // already has the child.
        let child_key = child
            .strip_prefix('[')
            .and_then(|c| c.strip_suffix(']'))
            .filter(|c| !c.contains(['[', ']']))
            .unwrap_or(child);
        let Value::Array(items) = params
            .entry(key.to_owned())
            .or_insert_with(|| Value::Array(vec![]))
        else {
            return Err(());
        };
        // `params_hash_has_key?`, by the child's first key.
        let first = child_key
            .trim_start_matches(['[', ']'])
            .split(['[', ']'])
            .next()
            .unwrap_or("");
        let reuse = matches!(items.last(), Some(Value::Object(last)) if !last.contains_key(first));
        if reuse {
            let Some(Value::Object(last)) = items.last_mut() else {
                return Err(());
            };
            normalize_params(last, child_key, value)?;
        } else {
            let mut hash = Map::new();
            normalize_params(&mut hash, child_key, value)?;
            items.push(Value::Object(hash));
        }
    } else {
        let Value::Object(nested) = params
            .entry(key.to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
        else {
            return Err(());
        };
        normalize_params(nested, after, value)?;
    }
    Ok(())
}

/// Nest each pair; `Err` with the name of one that conflicts.
fn nest_pairs(
    into: &mut serde_json::Map<String, serde_json::Value>,
    pairs: Vec<(String, String)>,
) -> Result<(), String> {
    for (name, value) in pairs {
        normalize_params(into, &name, serde_json::Value::String(value)).map_err(|()| name)?;
    }
    Ok(())
}

impl<S> FromRequest<S> for NestedParams
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        use serde_json::Value;
        let unprocessable = |e: String| (StatusCode::UNPROCESSABLE_ENTITY, e).into_response();
        let invalid = |name: String| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid parameter: {name}"),
            )
                .into_response()
        };
        let mut query_parameters = serde_json::Map::new();
        let query = req.uri().query().unwrap_or("").to_owned();
        nest_pairs(
            &mut query_parameters,
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect(),
        )
        .map_err(invalid)?;
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut request_parameters = serde_json::Map::new();
        if content_type.contains("application/json") {
            let bytes = axum::body::Bytes::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            if !bytes.is_empty() {
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(Value::Object(body)) => request_parameters = body,
                    // `ActionDispatch::Request#parse_formatted_parameters`.
                    Ok(other) => {
                        request_parameters.insert("_json".into(), other);
                    }
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
            nest_pairs(&mut request_parameters, pairs).map_err(invalid)?;
        } else {
            let bytes = axum::body::Bytes::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            nest_pairs(
                &mut request_parameters,
                url::form_urlencoded::parse(&bytes).into_owned().collect(),
            )
            .map_err(invalid)?;
        }
        // `request_parameters.merge(query_parameters)`.
        request_parameters.extend(query_parameters);
        Ok(NestedParams(Value::Object(request_parameters)))
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    fn nested(pairs: &[(&str, &str)]) -> Result<serde_json::Value, ()> {
        let mut params = serde_json::Map::new();
        for (name, value) in pairs {
            super::normalize_params(&mut params, name, json!(value))?;
        }
        Ok(serde_json::Value::Object(params))
    }

    #[test]
    fn booleans_cast_as_active_model_casts_them() {
        use super::rails::cast_bool;
        for falsy in [json!(false), json!(0), json!("0"), json!("f"), json!("F")] {
            assert_eq!(cast_bool(&falsy), Some(false), "{falsy}");
        }
        for falsy in ["false", "FALSE", "off", "OFF"] {
            assert_eq!(cast_bool(&json!(falsy)), Some(false), "{falsy}");
        }
        // Only the spellings ActiveModel lists are false.
        for truthy in ["1", "t", "true", "on", "yes", "False", "Off", "no"] {
            assert_eq!(cast_bool(&json!(truthy)), Some(true), "{truthy}");
        }
        assert_eq!(cast_bool(&json!("")), None);
        assert_eq!(cast_bool(&serde_json::Value::Null), None);
        assert_eq!(super::rails::cast_int(&json!("300")), Some(300));
        assert_eq!(super::rails::cast_int(&json!("12abc")), Some(12));
        assert_eq!(super::rails::cast_int(&json!("")), None);
    }

    #[test]
    fn form_names_nest_as_rack_nests_them() {
        assert_eq!(
            nested(&[
                ("subscription[endpoint]", "https://e"),
                ("subscription[keys][auth]", "a"),
                ("data[alerts][admin.sign_up]", "true"),
                ("ids[]", "1"),
                ("ids[]", "2"),
                ("f[][name]", "x"),
                ("f[][value]", "y"),
                ("f[][name]", "z"),
                ("plain", "p"),
            ])
            .unwrap(),
            json!({
                "subscription": {"endpoint": "https://e", "keys": {"auth": "a"}},
                "data": {"alerts": {"admin.sign_up": "true"}},
                "ids": ["1", "2"],
                "f": [{"name": "x", "value": "y"}, {"name": "z"}],
                "plain": "p",
            })
        );
        assert!(nested(&[("a", "1"), ("a[b]", "2")]).is_err());
    }
}
