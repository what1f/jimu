//! Cancellation-aware `AgentMiddleware` bridging Jimu tool approval into `InteractionRequest`.

use crate::harness::{ApprovalDecision, InteractionRequest, TuiEvent};
use futures_channel::{mpsc, oneshot};
use futures_util::FutureExt;
use jimu_agent::{AgentMiddleware, BeforeToolCallCtx, BeforeToolCallResult};
use jimu_ai::{BoxFuture, CancelToken};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

type Policy = dyn Fn(&BeforeToolCallCtx<'_>) -> bool + Send + Sync;

/// Shared event channel sink connecting [`JimuApprovalMiddleware`] (installed before
/// `KitSession` creation) with [`super::JimuHarness`].
#[derive(Clone)]
pub struct JimuInteractionSink {
    tx: Arc<Mutex<Option<mpsc::UnboundedSender<TuiEvent>>>>,
    pending: Arc<Mutex<Vec<TuiEvent>>>,
}

impl Default for JimuInteractionSink {
    fn default() -> Self {
        Self::new()
    }
}

impl JimuInteractionSink {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn attach(&self, sender: mpsc::UnboundedSender<TuiEvent>) {
        let buffered: Vec<TuiEvent> = {
            let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            *tx = Some(sender.clone());
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect()
        };
        for ev in buffered {
            let _ = sender.unbounded_send(ev);
        }
    }

    pub(crate) fn emit(&self, event: TuiEvent) {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(sender) = tx {
            let _ = sender.unbounded_send(event);
        } else {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        }
    }
}

/// Native `AgentMiddleware` that asks the TUI for approval before executing matching tool calls.
///
/// Create one instance per session so `AllowSession` grants stay scoped to that session.
pub struct JimuApprovalMiddleware {
    sink: JimuInteractionSink,
    policy: Box<Policy>,
    grants: Mutex<HashSet<String>>,
    denied_reason: String,
}

impl JimuApprovalMiddleware {
    /// Prompt before any tool call for which `policy(ctx)` returns `true`.
    pub fn new(
        sink: JimuInteractionSink,
        policy: impl Fn(&BeforeToolCallCtx<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            sink,
            policy: Box::new(policy),
            grants: Mutex::default(),
            denied_reason: "Permission denied".into(),
        }
    }

    /// Customize the error reason returned to the model when a call is denied.
    pub fn with_denied_reason(mut self, reason: impl Into<String>) -> Self {
        self.denied_reason = reason.into();
        self
    }
}

impl AgentMiddleware for JimuApprovalMiddleware {
    fn before_tool_call<'a>(
        &'a self,
        ctx: BeforeToolCallCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<BeforeToolCallResult>> {
        Box::pin(async move {
            let name = ctx.tool_call.name.clone();
            if !(self.policy)(&ctx)
                || self
                    .grants
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&name)
            {
                return None;
            }
            let summary = summarize_tool_args(ctx.args);
            let detail = (!ctx.args.is_null()
                && ctx.args.as_object().is_none_or(|m| !m.is_empty()))
            .then(|| {
                serde_json::to_string_pretty(ctx.args).unwrap_or_else(|_| ctx.args.to_string())
            });
            let (tx, rx) = oneshot::channel();
            self.sink
                .emit(TuiEvent::Interaction(InteractionRequest::ApproveTool {
                    tool_name: name.clone(),
                    summary,
                    detail,
                    reply: tx,
                }));

            let decision = futures_util::select_biased! {
                _ = cancel.cancelled().fuse() => ApprovalDecision::Deny,
                ans = rx.fuse() => ans.unwrap_or(ApprovalDecision::Deny),
            };
            match decision {
                ApprovalDecision::AllowOnce => None,
                ApprovalDecision::AllowSession => {
                    self.grants
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(name);
                    None
                }
                ApprovalDecision::Deny => Some(BeforeToolCallResult {
                    block: true,
                    reason: Some(self.denied_reason.clone()),
                    ..Default::default()
                }),
            }
        })
    }
}

fn summarize_tool_args(args: &serde_json::Value) -> String {
    if let Some(primary) = ["path", "command", "url", "pattern"]
        .into_iter()
        .find_map(|k| args.get(k).and_then(|v| v.as_str()))
    {
        return primary.to_owned();
    }
    let raw = args.to_string();
    if raw == "{}" || raw == "null" {
        return String::new();
    }
    let mut out: String = raw.chars().take(80).collect();
    if raw.chars().count() > 80 {
        out.push('…');
    }
    out
}
