//! Request reconstruction and source-local payload adaptation.
use super::{roles, ManualRequest, PreservedScopedSession, RecoveryWindow, RoleCapabilities};
use crate::model::Source;
use axum::{
    body::Body,
    http::{request::Parts, HeaderMap, Request, StatusCode},
};
use bytes::Bytes;
use serde_json::Value;

pub(super) fn clear_body_headers(headers: &mut HeaderMap) {
    for name in ["content-length", "content-md5", "digest"] {
        headers.remove(name);
    }
}

/// Preserve the same authenticated request context across internal replans.
pub(super) fn retry_request(parts: &Parts, bytes: Bytes) -> Request<Body> {
    let mut retry = Request::new(Body::from(bytes));
    *retry.method_mut() = parts.method.clone();
    *retry.uri_mut() = parts.uri.clone();
    *retry.headers_mut() = parts.headers.clone();
    if let Some(ingress) = parts.extensions.get::<crate::remote::RemoteIngress>() {
        retry.extensions_mut().insert(ingress.clone());
    }
    if let Some(ingress) = parts.extensions.get::<crate::chatbot::ChatbotIngress>() {
        retry.extensions_mut().insert(ingress.clone());
    }
    if let Some(session) = parts.extensions.get::<PreservedScopedSession>() {
        retry.extensions_mut().insert(session.clone());
    }
    if let Some(window) = parts.extensions.get::<RecoveryWindow>() {
        retry.extensions_mut().insert(window.clone());
    }
    if let Some(manual) = parts.extensions.get::<ManualRequest>() {
        retry.extensions_mut().insert(manual.clone());
    }
    if let Some(retries) = parts.extensions.get::<super::retries::RequestRetries>() {
        retry.extensions_mut().insert(retries.clone());
    }
    retry
}

/// Repairs stay local to one source; another source receives the original bytes.
pub(super) struct Payload<'a> {
    original: &'a Value,
    bytes: Bytes,
    compatibility_body: Option<Value>,
    compatibility_changes: Vec<String>,
    role_repaired: bool,
}

impl<'a> Payload<'a> {
    pub(super) fn new(
        parsed: &'a Value,
        bytes: &Bytes,
        compatibility: bool,
        source: &Source,
        capabilities: &RoleCapabilities,
    ) -> Self {
        let mut payload = Self {
            original: parsed,
            bytes: bytes.clone(),
            compatibility_body: compatibility.then(|| parsed.clone()),
            compatibility_changes: Vec::new(),
            role_repaired: false,
        };
        if capabilities.requires_developer(source) {
            if let Some(repaired) = roles::convert(parsed) {
                payload.repair_roles(repaired);
            }
        }
        payload
    }

    pub(super) fn may_repair(&self) -> bool {
        self.compatibility_body.is_some() || roles::has_system(self.original)
    }

    pub(super) fn role_repaired(&self) -> bool {
        self.role_repaired
    }

    fn repair_roles(&mut self, repaired: Value) {
        if let Some(body) = &mut self.compatibility_body {
            *body = repaired;
        } else {
            self.bytes = Bytes::from(serde_json::to_vec(&repaired).expect("JSON"));
        }
        self.role_repaired = true;
    }

    pub(super) fn wire(
        &self,
        official: bool,
        history_input: Option<&Value>,
    ) -> Result<(Bytes, Vec<String>), crate::chatbot::Invalid> {
        let (bytes, mut ignored) = match &self.compatibility_body {
            Some(body) => crate::chatbot::upstream_payload(body, official, history_input)
                .map(|(body, ignored)| (Bytes::from(body), ignored))?,
            None => (self.bytes.clone(), Vec::new()),
        };
        ignored.extend(self.compatibility_changes.iter().cloned());
        if self.role_repaired {
            ignored.push("input.role".into());
        }
        ignored.sort();
        ignored.dedup();
        Ok((bytes, ignored))
    }

    pub(super) fn repair_rejection(
        &mut self,
        status: StatusCode,
        rejection: &Value,
        source: &Source,
        capabilities: &RoleCapabilities,
    ) -> bool {
        if !self.role_repaired && matches!(status.as_u16(), 400 | 422) {
            let body = self.compatibility_body.as_ref().unwrap_or(self.original);
            if let Some(repaired) = roles::adapt(body, rejection) {
                capabilities.remember(source);
                self.repair_roles(repaired);
                return true;
            }
        }
        if status == StatusCode::BAD_REQUEST && self.compatibility_changes.len() < 3 {
            if let Some(body) = &mut self.compatibility_body {
                if let Some(field) = crate::chatbot::repair(body, rejection) {
                    self.compatibility_changes.push(field);
                    return true;
                }
            }
        }
        false
    }
}
