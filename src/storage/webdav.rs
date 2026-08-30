use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use percent_encoding::percent_decode_str;
use quick_xml::Reader;
use quick_xml::events::{BytesCData, BytesText, Event};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, header};
use std::collections::{BTreeSet, HashSet};
use std::io::{Error, ErrorKind};
use std::time::Duration;
use url::Url;

use super::FS;

const MAX_CONCURRENT_PROPFIND: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getetag/></d:prop></d:propfind>"#;

/// Configuration for a WebDAV collection used as a GIB storage.
///
/// The password is intentionally kept out of `Debug` implementations and
/// command output. Repository encryption passwords are separate from this
/// transport credential.
pub struct WebDavFSConfig {
    pub url: String,
    pub username: String,
    pub password: String,
}

pub struct WebDavFS {
    client: Client,
    root_url: Url,
    root_segments: Vec<String>,
    username: String,
    password: String,
}

#[derive(Debug, Default)]
struct ParsedDavResource {
    href: String,
    is_collection: bool,
    etag: Option<String>,
    status: Option<String>,
    statuses: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DavResource {
    path: String,
    is_collection: bool,
    etag: Option<String>,
}

#[derive(Clone, Copy)]
enum ResponseField {
    Href,
    Etag,
    Status,
}

impl std::fmt::Debug for WebDavFSConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebDavFSConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

impl std::fmt::Debug for WebDavFS {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebDavFS")
            .field("root_url", &self.root_url)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

impl WebDavFS {
    pub fn new(config: WebDavFSConfig) -> Result<Self, String> {
        Self::from_config(config)
    }

    fn from_config(config: WebDavFSConfig) -> Result<Self, String> {
        validate_credentials(&config)?;
        let parsed_url = Url::parse(config.url.trim())
            .map_err(|error| format!("Invalid WebDAV URL: {}", error))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err("WebDAV URL must use http:// or https://".to_string());
        }
        Self::from_validated_config(config, parsed_url)
    }

    fn from_validated_config(config: WebDavFSConfig, parsed_url: Url) -> Result<Self, String> {
        if parsed_url.host_str().is_none() {
            return Err("WebDAV URL must include a host".to_string());
        }
        if !parsed_url.username().is_empty() || parsed_url.password().is_some() {
            return Err("WebDAV URL must not contain embedded credentials".to_string());
        }
        if parsed_url.query().is_some() {
            return Err("WebDAV URL must not contain a query string".to_string());
        }
        if parsed_url.fragment().is_some() {
            return Err("WebDAV URL must not contain a fragment".to_string());
        }

        let root_url = normalize_root_url(parsed_url)?;
        let root_segments = decode_url_segments(root_url.path())
            .map_err(|error| format!("Invalid WebDAV URL path: {}", error))?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(format!("gib/{}", crate::VERSION))
            .build()
            .map_err(|error| format!("Failed to initialize the WebDAV client: {}", error))?;

        Ok(Self {
            client,
            root_url,
            root_segments,
            username: config.username,
            password: config.password,
        })
    }

    /// Checks that the configured URL is an authenticated WebDAV collection.
    /// This performs no write and is used before a storage configuration is
    /// persisted.
    pub async fn validate_root(&self) -> Result<(), std::io::Error> {
        let resources = self.propfind("", 0).await?;
        let root = resources.iter().find(|resource| resource.path.is_empty());

        match root {
            Some(resource) if resource.is_collection => Ok(()),
            Some(_) => Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV URL points to a file, not a collection",
            )),
            None => Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV response did not identify the configured root collection",
            )),
        }
    }

    fn authenticated(&self, request: RequestBuilder) -> RequestBuilder {
        request.basic_auth(&self.username, Some(&self.password))
    }

    fn resource_url(&self, path: &str) -> Result<Url, std::io::Error> {
        let path = normalize_relative_path(path)?;
        let mut url = self.root_url.clone();

        if !path.is_empty() {
            let mut segments = url.path_segments_mut().map_err(|_| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "WebDAV URL cannot be used as a hierarchical collection",
                )
            })?;
            segments.pop_if_empty();
            for segment in path.split('/') {
                segments.push(segment);
            }
        }

        Ok(url)
    }

    async fn send_request(
        &self,
        request: RequestBuilder,
        operation: &str,
    ) -> Result<Response, std::io::Error> {
        let response = request
            .send()
            .await
            .map_err(|error| request_error(error, operation))?;

        if response.status().is_redirection() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "WebDAV {} was redirected; configure the final collection URL because redirects are not followed automatically",
                    operation
                ),
            ));
        }

        Ok(response)
    }

    async fn propfind(&self, path: &str, depth: u8) -> Result<Vec<DavResource>, std::io::Error> {
        let url = self.resource_url(path)?;
        let method = Method::from_bytes(b"PROPFIND")
            .map_err(|error| Error::new(ErrorKind::InvalidInput, error.to_string()))?;
        let request = self
            .authenticated(self.client.request(method, url))
            .header("Depth", depth.to_string())
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(PROPFIND_BODY);
        let response = self.send_request(request, "PROPFIND").await?;
        let status = response.status();

        if status == StatusCode::NOT_FOUND {
            return Err(status_error(status, "PROPFIND"));
        }
        if status != StatusCode::MULTI_STATUS {
            return Err(status_error(status, "PROPFIND"));
        }

        let body = response
            .bytes()
            .await
            .map_err(|error| request_error(error, "PROPFIND"))?;
        let parsed = parse_multistatus(&body)?;
        parsed
            .into_iter()
            .map(|resource| self.canonicalize_resource(resource))
            .filter_map(|resource| match resource {
                Ok(Some(resource)) => Some(Ok(resource)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    async fn propfind_collection(&self, path: &str) -> Result<Vec<DavResource>, std::io::Error> {
        match self.propfind(path, 1).await {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            result => result,
        }
    }

    fn canonicalize_resource(
        &self,
        resource: ParsedDavResource,
    ) -> Result<Option<DavResource>, std::io::Error> {
        let href = resource.href.trim();
        if href.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "WebDAV response contained an empty resource URL",
            ));
        }

        let candidate = match Url::parse(href) {
            Ok(url) => url,
            Err(_) => self.root_url.join(href).map_err(|error| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("Invalid WebDAV resource URL: {}", error),
                )
            })?,
        };

        if candidate.query().is_some() || candidate.fragment().is_some() {
            return Ok(None);
        }
        if !same_origin(&self.root_url, &candidate) {
            return Ok(None);
        }

        let candidate_segments = decode_url_segments(candidate.path()).map_err(|error| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Invalid WebDAV resource path: {}", error),
            )
        })?;

        if candidate_segments.len() < self.root_segments.len()
            || candidate_segments[..self.root_segments.len()] != self.root_segments[..]
        {
            return Ok(None);
        }

        let relative_segments = &candidate_segments[self.root_segments.len()..];
        let path = relative_segments.join("/");
        Ok(Some(DavResource {
            path,
            is_collection: resource.is_collection,
            etag: usable_etag(resource.etag.as_deref()),
        }))
    }

    async fn ensure_parent_collections(&self, path: &str) -> Result<(), std::io::Error> {
        let path = normalize_relative_path(path)?;
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        let file_name = segments.next_back();
        if file_name.is_none() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV write path cannot be empty",
            ));
        }

        let mut parent = String::new();
        for segment in segments {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(segment);
            self.create_collection(&parent).await?;
        }
        Ok(())
    }

    async fn create_collection(&self, path: &str) -> Result<(), std::io::Error> {
        let url = self.resource_url(path)?;
        let method = Method::from_bytes(b"MKCOL")
            .map_err(|error| Error::new(ErrorKind::InvalidInput, error.to_string()))?;
        let response = self
            .send_request(
                self.authenticated(self.client.request(method, url)),
                "MKCOL",
            )
            .await?;

        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            // Existing collections commonly report 405 for MKCOL. Treat it
            // as success so concurrent writers can create the same hierarchy.
            return Ok(());
        }
        ensure_success(response, "MKCOL").map(|_| ())
    }
}

#[async_trait]
impl FS for WebDavFS {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error> {
        let url = self.resource_url(path)?;
        let response = self
            .send_request(self.authenticated(self.client.get(url)), "GET")
            .await?;
        let response = ensure_success(response, "GET")?;
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| request_error(error, "GET"))
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error> {
        let path = normalize_relative_path(path)?;
        self.ensure_parent_collections(&path).await?;
        let url = self.resource_url(&path)?;
        let response = self
            .send_request(
                self.authenticated(self.client.put(url)).body(data.to_vec()),
                "PUT",
            )
            .await?;
        ensure_success(response, "PUT").map(|_| ())
    }

    async fn list_files(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        let root_path = normalize_relative_path(path)?;
        let mut pending = vec![root_path.clone()];
        let mut visited = HashSet::from([root_path]);
        let mut files = BTreeSet::new();

        while !pending.is_empty() {
            let batch = std::mem::take(&mut pending);
            let responses = stream::iter(batch.into_iter().map(|collection_path| async move {
                let resources = self.propfind_collection(&collection_path).await;
                (collection_path, resources)
            }))
            .buffer_unordered(MAX_CONCURRENT_PROPFIND)
            .collect::<Vec<_>>()
            .await;

            for (collection_path, resources) in responses {
                for resource in resources? {
                    if resource.path == collection_path {
                        continue;
                    }
                    if !is_descendant_path(&resource.path, &collection_path) {
                        continue;
                    }
                    if resource.is_collection {
                        if visited.insert(resource.path.clone()) {
                            pending.push(resource.path);
                        }
                    } else {
                        files.insert(resource.path);
                    }
                }
            }
        }

        Ok(files.into_iter().collect())
    }

    async fn delete_file(&self, path: &str) -> Result<(), std::io::Error> {
        let url = self.resource_url(path)?;
        let response = self
            .send_request(self.authenticated(self.client.delete(url)), "DELETE")
            .await?;
        ensure_success(response, "DELETE").map(|_| ())
    }

    async fn read_file_with_version(
        &self,
        path: &str,
    ) -> Result<(Vec<u8>, String), std::io::Error> {
        let path = normalize_relative_path(path)?;
        if path.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV versioned reads require a file path",
            ));
        }

        let resources = self.propfind(&path, 0).await?;
        let resource = resources
            .into_iter()
            .find(|resource| resource.path == path)
            .ok_or_else(|| status_error(StatusCode::NOT_FOUND, "PROPFIND"))?;
        if resource.is_collection {
            return Err(Error::new(
                ErrorKind::IsADirectory,
                "WebDAV path is a collection, not a file",
            ));
        }

        let url = self.resource_url(&path)?;
        let response = self
            .send_request(self.authenticated(self.client.get(url)), "GET")
            .await?;
        let response = ensure_success(response, "GET")?;
        let header_etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| usable_etag(Some(value)));
        let data = response
            .bytes()
            .await
            .map_err(|error| request_error(error, "GET"))?;
        let version = header_etag.or(resource.etag).ok_or_else(|| {
            Error::new(
                ErrorKind::Unsupported,
                "WebDAV server did not provide a usable ETag for the file",
            )
        })?;

        Ok((data.to_vec(), version))
    }

    async fn write_file_if_version(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<&str>,
    ) -> Result<(), std::io::Error> {
        let path = normalize_relative_path(path)?;
        self.ensure_parent_collections(&path).await?;
        let url = self.resource_url(&path)?;
        let request = self.authenticated(self.client.put(url));
        let request = match expected_version {
            Some(version) => request.header(header::IF_MATCH, version),
            None => request.header(header::IF_NONE_MATCH, "*"),
        };
        let response = self
            .send_request(request.body(data.to_vec()), "conditional PUT")
            .await?;
        ensure_success(response, "conditional PUT").map(|_| ())
    }
}

fn validate_credentials(config: &WebDavFSConfig) -> Result<(), String> {
    if config.url.trim().is_empty() {
        return Err("WebDAV URL cannot be empty".to_string());
    }
    if config.username.is_empty() {
        return Err("WebDAV username cannot be empty".to_string());
    }
    if config.password.is_empty() {
        return Err("WebDAV password cannot be empty".to_string());
    }
    Ok(())
}

fn normalize_root_url(mut url: Url) -> Result<Url, String> {
    let path = url.path().to_string();
    if path.is_empty() {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "WebDAV URL cannot be used as a hierarchical collection".to_string())?;
        segments.push("");
        drop(segments);
        return Ok(url);
    }
    if path == "/" {
        return Ok(url);
    }

    let trailing_empty_segments = path
        .split('/')
        .rev()
        .take_while(|segment| segment.is_empty())
        .count();
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| "WebDAV URL cannot be used as a hierarchical collection".to_string())?;

    if trailing_empty_segments == 0 {
        segments.push("");
    } else {
        for _ in 1..trailing_empty_segments {
            segments.pop_if_empty();
        }
    }
    drop(segments);
    Ok(url)
}

fn normalize_relative_path(path: &str) -> Result<String, std::io::Error> {
    if path.is_empty() {
        return Ok(String::new());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "WebDAV paths must be relative to the configured collection",
        ));
    }

    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains('\0') {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV paths cannot contain parent traversal or NUL characters",
            ));
        }
        segments.push(segment);
    }
    Ok(segments.join("/"))
}

fn decode_url_segments(path: &str) -> Result<Vec<String>, String> {
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = percent_decode_str(segment)
            .decode_utf8()
            .map_err(|_| "URL path contains invalid UTF-8".to_string())?
            .into_owned();
        if decoded == "."
            || decoded == ".."
            || decoded.contains('/')
            || decoded.contains('\\')
            || decoded.contains('\0')
        {
            return Err("URL path contains an unsafe segment separator or traversal".to_string());
        }
        segments.push(decoded);
    }
    Ok(segments)
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_descendant_path(path: &str, collection: &str) -> bool {
    collection.is_empty()
        || path == collection
        || path
            .strip_prefix(collection)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn usable_etag(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value == "*" {
        None
    } else {
        Some(value.to_string())
    }
}

fn request_error(error: reqwest::Error, operation: &str) -> std::io::Error {
    let message = if error.is_timeout() {
        format!("WebDAV {} timed out", operation)
    } else if error.is_connect() {
        format!("WebDAV {} could not connect to the server", operation)
    } else {
        format!("WebDAV {} request failed: {}", operation, error)
    };
    Error::new(ErrorKind::Other, message)
}

fn ensure_success(response: Response, operation: &str) -> Result<Response, std::io::Error> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(status_error(response.status(), operation))
    }
}

fn status_error(status: StatusCode, operation: &str) -> std::io::Error {
    let kind = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ErrorKind::PermissionDenied,
        StatusCode::NOT_FOUND => ErrorKind::NotFound,
        StatusCode::PRECONDITION_FAILED => ErrorKind::AlreadyExists,
        _ => ErrorKind::Other,
    };
    let detail = match status {
        StatusCode::UNAUTHORIZED => {
            "authentication failed; check the WebDAV username and app password"
        }
        StatusCode::FORBIDDEN => "the WebDAV account is not authorized for this collection",
        StatusCode::NOT_FOUND => "the WebDAV resource or collection was not found",
        StatusCode::CONFLICT => "the WebDAV collection is missing a parent or reported a conflict",
        StatusCode::PRECONDITION_FAILED => {
            "the WebDAV resource changed before the conditional write"
        }
        _ => "the WebDAV server rejected the request",
    };
    Error::new(
        kind,
        format!(
            "WebDAV {} failed with HTTP {}: {}",
            operation, status, detail
        ),
    )
}

fn parse_multistatus(xml: &[u8]) -> Result<Vec<ParsedDavResource>, std::io::Error> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut resources = Vec::new();
    let mut current: Option<ParsedDavResource> = None;
    let mut field = None;
    let mut in_resource_type = false;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => {
                let element_name = element.name();
                let local = local_name(element_name.as_ref());
                match local {
                    b"response" => {
                        if current.is_some() {
                            return Err(Error::new(
                                ErrorKind::InvalidData,
                                "Malformed WebDAV XML: nested response element",
                            ));
                        }
                        current = Some(ParsedDavResource::default());
                    }
                    b"href" if current.is_some() => field = Some(ResponseField::Href),
                    b"getetag" if current.is_some() => field = Some(ResponseField::Etag),
                    b"status" if current.is_some() => field = Some(ResponseField::Status),
                    b"resourcetype" if current.is_some() => in_resource_type = true,
                    b"collection" if in_resource_type => {
                        if let Some(resource) = current.as_mut() {
                            resource.is_collection = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(element)) => {
                let element_name = element.name();
                let local = local_name(element_name.as_ref());
                if local == b"collection" && in_resource_type {
                    if let Some(resource) = current.as_mut() {
                        resource.is_collection = true;
                    }
                }
            }
            Ok(Event::Text(text)) => {
                append_field_text(&mut current, field, text)?;
            }
            Ok(Event::CData(text)) => {
                append_cdata_text(&mut current, field, text)?;
            }
            Ok(Event::End(element)) => {
                let element_name = element.name();
                let local = local_name(element_name.as_ref());
                match local {
                    b"href" | b"getetag" => field = None,
                    b"status" => {
                        if let Some(resource) = current.as_mut() {
                            if let Some(status) = resource.status.take() {
                                resource.statuses.extend(
                                    status
                                        .split_whitespace()
                                        .nth(1)
                                        .and_then(|code| code.parse::<u16>().ok()),
                                );
                            }
                        }
                        field = None;
                    }
                    b"resourcetype" => in_resource_type = false,
                    b"response" => {
                        let resource = current.take().ok_or_else(|| {
                            Error::new(
                                ErrorKind::InvalidData,
                                "Malformed WebDAV XML: response ended without a start",
                            )
                        })?;
                        if resource.href.trim().is_empty() {
                            return Err(Error::new(
                                ErrorKind::InvalidData,
                                "Malformed WebDAV XML: response has no href",
                            ));
                        }
                        if !resource.statuses.is_empty()
                            && resource
                                .statuses
                                .iter()
                                .all(|status| !(200..300).contains(status))
                        {
                            continue;
                        }
                        resources.push(resource);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if current.is_some() || in_resource_type {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "Malformed WebDAV XML: response was not closed",
                    ));
                }
                return Ok(resources);
            }
            Ok(_) => {}
            Err(error) => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("Malformed WebDAV XML: {}", error),
                ));
            }
        }
        buffer.clear();
    }
}

fn append_field_text(
    current: &mut Option<ParsedDavResource>,
    field: Option<ResponseField>,
    text: BytesText<'_>,
) -> Result<(), std::io::Error> {
    let text = text.unescape().map_err(|error| {
        Error::new(
            ErrorKind::InvalidData,
            format!("Malformed WebDAV XML text: {}", error),
        )
    })?;
    append_field_value(current, field, text.as_ref());
    Ok(())
}

fn append_cdata_text(
    current: &mut Option<ParsedDavResource>,
    field: Option<ResponseField>,
    text: BytesCData<'_>,
) -> Result<(), std::io::Error> {
    let text = std::str::from_utf8(text.as_ref()).map_err(|error| {
        Error::new(
            ErrorKind::InvalidData,
            format!("Malformed WebDAV XML text: {}", error),
        )
    })?;
    append_field_value(current, field, text);
    Ok(())
}

fn append_field_value(
    current: &mut Option<ParsedDavResource>,
    field: Option<ResponseField>,
    value: &str,
) {
    let Some(resource) = current.as_mut() else {
        return;
    };
    match field {
        Some(ResponseField::Href) => resource.href.push_str(value),
        Some(ResponseField::Etag) => {
            resource
                .etag
                .get_or_insert_with(String::new)
                .push_str(value);
        }
        Some(ResponseField::Status) => resource
            .status
            .get_or_insert_with(String::new)
            .push_str(value),
        None => {}
    }
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    fn test_client(url: &str) -> WebDavFS {
        WebDavFS::new(WebDavFSConfig {
            url: url.to_string(),
            username: "user".to_string(),
            password: "secret".to_string(),
        })
        .expect("test URL should be valid")
    }

    #[test]
    fn normalizes_root_without_double_encoding() {
        let webdav = test_client("https://example.test/dav/root///");
        assert_eq!(webdav.root_url.as_str(), "https://example.test/dav/root/");

        let encoded = test_client("https://example.test/space%20name");
        assert_eq!(
            encoded.root_url.as_str(),
            "https://example.test/space%20name/"
        );

        let host_only = test_client("https://example.test");
        assert_eq!(host_only.root_url.as_str(), "https://example.test/");
    }

    #[test]
    fn accepts_http_and_https_root_urls() {
        let http = test_client("http://example.test/dav");
        let https = test_client("https://example.test/dav");
        assert_eq!(http.root_url.as_str(), "http://example.test/dav/");
        assert_eq!(https.root_url.as_str(), "https://example.test/dav/");
    }

    #[test]
    fn encodes_resource_path_segments() {
        let webdav = test_client("https://example.test/dav/");
        let url = webdav
            .resource_url("space name/#?%.txt")
            .expect("resource URL should be valid");
        assert_eq!(
            url.as_str(),
            "https://example.test/dav/space%20name/%23%3F%25.txt"
        );
    }

    #[test]
    fn rejects_unsafe_root_urls() {
        for url in [
            "ftp://example.test/dav/",
            "https://example.test/dav/?tenant=one",
            "https://example.test/dav/#fragment",
            "https://user:password@example.test/dav/",
        ] {
            let error = WebDavFS::new(WebDavFSConfig {
                url: url.to_string(),
                username: "user".to_string(),
                password: "secret".to_string(),
            })
            .expect_err("URL should be rejected");
            assert!(!error.contains("password"));
        }
    }

    #[test]
    fn parses_namespaced_multistatus_and_preserves_etag() {
        let xml = br#"
            <d:multistatus xmlns:d="DAV:">
              <d:response>
                <d:href>/dav/root/</d:href>
                <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
              </d:response>
              <d:response>
                <d:href>/dav/root/hello%20world.txt</d:href>
                <d:propstat><d:prop><d:resourcetype/><d:getetag>&quot;v1&quot;</d:getetag></d:prop></d:propstat>
              </d:response>
              <d:response>
                <d:href>/dav/root/hello%20world.txt</d:href>
                <d:propstat><d:prop><d:getetag>&quot;v1&quot;</d:getetag></d:prop></d:propstat>
              </d:response>
            </d:multistatus>
        "#;
        let parsed = parse_multistatus(xml).expect("XML should parse");
        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].is_collection);
        assert_eq!(parsed[1].etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn ignores_resources_outside_the_configured_root() {
        let webdav = test_client("https://example.test/dav/root/");
        let parsed = ParsedDavResource {
            href: "/dav/other/secret.txt".to_string(),
            is_collection: false,
            etag: Some("\"secret\"".to_string()),
            status: None,
            statuses: Vec::new(),
        };
        assert!(
            webdav
                .canonicalize_resource(parsed)
                .expect("resource should parse")
                .is_none()
        );
    }

    #[test]
    fn rejects_malformed_multistatus() {
        let error = parse_multistatus(b"<multistatus><response><href>/root").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn skips_resources_with_only_failed_property_statuses() {
        let xml = br#"
            <d:multistatus xmlns:d="DAV:">
              <d:response>
                <d:href>/dav/root/missing.txt</d:href>
                <d:propstat>
                  <d:prop><d:getetag/></d:prop>
                  <d:status>HTTP/1.1 404 Not Found</d:status>
                </d:propstat>
              </d:response>
            </d:multistatus>
        "#;
        assert!(parse_multistatus(xml).expect("XML should parse").is_empty());
    }

    #[test]
    fn maps_conditional_status_to_already_exists() {
        let error = status_error(StatusCode::PRECONDITION_FAILED, "conditional PUT");
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    }

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
    }

    async fn spawn_webdav_test_server() -> (
        String,
        Arc<Mutex<Vec<RecordedRequest>>>,
        oneshot::Sender<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server should bind");
        let address = listener
            .local_addr()
            .expect("test server should have address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_task = Arc::clone(&requests);
        let (stop_sender, mut stop_receiver) = oneshot::channel();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_receiver => break,
                    result = listener.accept() => {
                        let (socket, _) = result.expect("test server should accept");
                        let requests = Arc::clone(&requests_for_task);
                        tokio::spawn(async move {
                            handle_webdav_test_connection(socket, requests).await;
                        });
                    }
                }
            }
        });

        (
            format!("http://{}/dav/root/", address),
            requests,
            stop_sender,
        )
    }

    async fn handle_webdav_test_connection(
        mut socket: tokio::net::TcpStream,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    ) {
        let mut buffer = Vec::new();
        let header_end;
        loop {
            let mut chunk = [0_u8; 4096];
            let bytes_read = socket
                .read(&mut chunk)
                .await
                .expect("test server should read");
            if bytes_read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..bytes_read]);
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = end + 4;
                break;
            }
        }

        let header_text = String::from_utf8_lossy(&buffer[..header_end]);
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().expect("request line should exist");
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default().to_string();
        let target = request_parts.next().unwrap_or_default().to_string();
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect::<Vec<_>>();
        requests
            .lock()
            .expect("request lock should not be poisoned")
            .push(RecordedRequest {
                method: method.clone(),
                target: target.clone(),
                headers: headers.clone(),
            });

        let depth = header_value(&headers, "depth");
        let response = match (method.as_str(), target.as_str(), depth.as_deref()) {
            ("PROPFIND", "/dav/root/", Some("0")) => ("207 Multi-Status", "", root_multistatus()),
            ("PROPFIND", "/dav/root/", Some("1")) => {
                ("207 Multi-Status", "", root_children_multistatus())
            }
            ("PROPFIND", "/dav/root/dir/", Some("1")) => {
                ("207 Multi-Status", "", directory_multistatus())
            }
            ("PROPFIND", "/dav/root/dir/file.txt", Some("0")) => (
                "207 Multi-Status",
                "",
                file_multistatus("/dav/root/dir/file.txt", "\"file-v1\""),
            ),
            ("MKCOL", "/dav/root/dir/", _) => ("405 Method Not Allowed", "", String::new()),
            ("PUT", "/dav/root/dir/new%20file.txt", _) => {
                assert_eq!(
                    header_value(&headers, "authorization").as_deref(),
                    Some("Basic dXNlcjpzZWNyZXQ=")
                );
                assert_eq!(
                    header_value(&headers, "if-none-match").as_deref(),
                    Some("*")
                );
                ("201 Created", "", String::new())
            }
            ("PUT", "/dav/root/dir/file.txt", _) => {
                assert_eq!(
                    header_value(&headers, "if-match").as_deref(),
                    Some("\"file-v1\"")
                );
                ("204 No Content", "", String::new())
            }
            ("GET", "/dav/root/dir/file.txt", _) => (
                "200 OK",
                "ETag: \"file-v1\"\r\n",
                "file contents".to_string(),
            ),
            ("DELETE", "/dav/root/dir/file.txt", _) => ("204 No Content", "", String::new()),
            _ => ("404 Not Found", "", String::new()),
        };

        let response_text = format!(
            "HTTP/1.1 {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.0,
            response.1,
            response.2.len(),
            response.2
        );
        socket
            .write_all(response_text.as_bytes())
            .await
            .expect("test server should write");
    }

    fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.clone())
    }

    fn root_multistatus() -> String {
        "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/dav/root/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response></d:multistatus>".to_string()
    }

    fn root_children_multistatus() -> String {
        "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/dav/root/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response><d:response><d:href>/dav/root/dir/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response><d:response><d:href>/dav/root/dir/file.txt</d:href><d:propstat><d:prop><d:resourcetype/><d:getetag>&quot;file-v1&quot;</d:getetag></d:prop></d:propstat></d:response></d:multistatus>".to_string()
    }

    fn directory_multistatus() -> String {
        "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/dav/root/dir/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response><d:response><d:href>/dav/root/dir/file.txt</d:href><d:propstat><d:prop><d:resourcetype/><d:getetag>&quot;file-v1&quot;</d:getetag></d:prop></d:propstat></d:response></d:multistatus>".to_string()
    }

    fn file_multistatus(path: &str, etag: &str) -> String {
        format!(
            "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>{path}</d:href><d:propstat><d:prop><d:resourcetype/><d:getetag>{etag}</d:getetag></d:prop></d:propstat></d:response></d:multistatus>"
        )
    }

    #[tokio::test]
    #[ignore = "requires loopback socket permissions"]
    async fn performs_authenticated_recursive_and_conditional_operations() {
        let (url, requests, stop_sender) = spawn_webdav_test_server().await;
        let webdav = WebDavFS::new(WebDavFSConfig {
            url,
            username: "user".to_string(),
            password: "secret".to_string(),
        })
        .expect("test WebDAV client should initialize");

        webdav
            .validate_root()
            .await
            .expect("root collection should validate");
        assert_eq!(
            webdav
                .list_files("")
                .await
                .expect("recursive listing should succeed"),
            vec!["dir/file.txt".to_string()]
        );
        let (contents, version) = webdav
            .read_file_with_version("dir/file.txt")
            .await
            .expect("versioned read should succeed");
        assert_eq!(contents, b"file contents");
        assert_eq!(version, "\"file-v1\"");
        webdav
            .write_file_if_version("dir/new file.txt", b"new", None)
            .await
            .expect("create conditional write should succeed");
        webdav
            .write_file_if_version("dir/file.txt", b"updated", Some("\"file-v1\""))
            .await
            .expect("matching conditional write should succeed");
        webdav
            .delete_file("dir/file.txt")
            .await
            .expect("delete should succeed");

        let requests = requests
            .lock()
            .expect("request lock should not be poisoned");
        assert!(requests.iter().all(|request| {
            header_value(&request.headers, "authorization").as_deref()
                == Some("Basic dXNlcjpzZWNyZXQ=")
        }));
        assert!(requests.iter().any(|request| {
            request.method == "PROPFIND"
                && request.target == "/dav/root/"
                && header_value(&request.headers, "depth").as_deref() == Some("1")
        }));
        assert!(
            requests
                .iter()
                .any(|request| { request.method == "MKCOL" && request.target == "/dav/root/dir/" })
        );
        drop(requests);
        stop_sender.send(()).expect("test server should stop");
    }
}
