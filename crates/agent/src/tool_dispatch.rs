//! Tool gating, dispatch and policy bookkeeping for the agent loop.
//!
//! These methods used to live inline in `agent_loop.rs`. They are moved
//! without behaviour changes so the main loop stays about streaming and
//! turn accounting.

use super::*;

impl Agent {
    ///
    /// Returns `Some` when the batch was interrupted by an abort. Results are
    /// replayed in call order whether or not they ran concurrently, so the
    /// conversation never depends on which tool finished first.
    pub(super) fn run_tool_batch(
        &mut self,
        calls: &[ToolCall],
        turn: usize,
        sink: &mut dyn EventSink,
    ) -> Option<FinishReason> {
        let mut remaining = turn_budget_state();
        // Gates are decisions, not side effects on the workspace: they run
        // on this thread in call order before the workers spawn. A batch with
        // hooks or a policy therefore keeps the same parallel throughput as
        // an ungated one.
        if calls.len() > 1 && calls.iter().all(|call| self.parallel_safe(&call.name)) {
            return self.run_parallel_batch(calls, turn, sink, &mut remaining);
        }
        for (index, call) in calls.iter().enumerate() {
            if self.abort.is_aborted() {
                // 当前调用和它后面的调用都没执行；补完结果再退出。
                self.record_unrun_calls(&calls[index..], UNRUN_INTERRUPTED, false, sink);
                sink.emit(AgentEvent::Aborted);
                return Some(FinishReason::Aborted);
            }
            if self.has_steering() {
                // Answer the calls that will not run before appending the
                // correction, then end this batch so the next request sees it.
                self.record_unrun_calls(&calls[index..], UNRUN_STEERED, false, sink);
                self.inject_steering(sink);
                return None;
            }
            if let Some(warning) = self.duplicate_warning(call, turn) {
                self.record_duplicate(call, warning, sink);
                continue;
            }
            let outcome = self.execute_tool(call, sink);
            self.note_tool_outcome(call, &outcome, turn);
            // The operator watched the full result arrive; the model gets a
            // bounded copy. A single large `read` or `bash` would otherwise
            // stay in the transcript and be paid for on every later turn.
            let content = bound_tool_output(
                &call.name,
                &outcome.content,
                outcome.is_error,
                &mut remaining,
            );
            // Same builder as the parallel path: a single tool call (the
            // common case) and every batch that carries a gate take this
            // branch, and a vision tool's images must survive here too.
            self.push_history(tool_result_message(call, &outcome, content));
            // A policy that refuses the same call twice ends the run: the
            // model is not going to find another way, and burning the rest
            // of the turn budget on one denied path helps nobody.
            if let Some(reason) = self.policy_stop.take() {
                // 被拦下的这次调用已经有结果；后面的调用还没跑，补一条。
                self.record_unrun_calls(&calls[index + 1..], UNRUN_POLICY, false, sink);
                let reason = FinishReason::PolicyBlocked(reason);
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Some(reason);
            }
        }
        None
    }

    /// Run read-only calls in parallel, up to [`MAX_PARALLEL_TOOLS`] at a time.
    ///
    /// The tools are read-only and the registry is shared behind an `Arc`, so
    /// each worker gets the same instances and its own copy of the context.
    fn run_parallel_batch(
        &mut self,
        calls: &[ToolCall],
        turn: usize,
        sink: &mut dyn EventSink,
        remaining: &mut usize,
    ) -> Option<FinishReason> {
        // Every call is announced before any completes, so the window shows
        // the whole batch as pending rather than one row at a time.
        for call in calls {
            sink.emit(AgentEvent::ToolRequested(call.clone()));
        }

        // 已记录结果的调用数，用于中止时找出还没跑的部分。
        let mut done = 0usize;
        for chunk in calls.chunks(MAX_PARALLEL_TOOLS) {
            if self.abort.is_aborted() {
                // 整批在开始时就宣告过了；未完成的部分补 ToolFinished + 结果。
                self.record_unrun_calls(&calls[done..], UNRUN_INTERRUPTED, true, sink);
                sink.emit(AgentEvent::Aborted);
                return Some(FinishReason::Aborted);
            }
            if self.has_steering() {
                self.record_unrun_calls(&calls[done..], UNRUN_STEERED, true, sink);
                self.inject_steering(sink);
                return None;
            }

            // Decide what runs before spawning: an exact repeat of an earlier
            // call, or of an earlier call in this same batch, is answered with
            // a notice instead of touching the disk again.
            let mut skip: Vec<Option<ToolOutcome>> = Vec::with_capacity(chunk.len());
            let mut pending: Vec<String> = Vec::new();
            for call in chunk {
                // The gate is first: a policy refusal is a decision about the
                // call, not about whether its result happens to be cached.
                if let Some(outcome) = self.gate_decision(call, &call.parsed_args()) {
                    skip.push(Some(outcome));
                    continue;
                }
                if let Some(key) = repeat_key(call) {
                    if pending.iter().any(|seen| seen == &key) {
                        skip.push(Some(ToolOutcome {
                            content: format!(
                                "[duplicate call skipped] {} was already requested with the same \
                                 arguments in this batch; its result is in this turn's tool results.",
                                call.name
                            ),
                            is_error: true,
                            images: Vec::new(),
                        }));
                        continue;
                    }
                    if let Some(warning) = self.duplicate_warning(call, turn) {
                        skip.push(Some(ToolOutcome {
                            content: warning,
                            is_error: true,
                            images: Vec::new(),
                        }));
                        continue;
                    }
                    pending.push(key);
                }
                skip.push(None);
            }

            let registry = Arc::clone(&self.registry);
            let ctx = self.ctx.clone();
            let mut outcomes: Vec<Option<(ToolOutcome, u64)>> =
                (0..chunk.len()).map(|_| None).collect();
            std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| skip[*index].is_none())
                    .map(|(index, call)| {
                        let registry = Arc::clone(&registry);
                        let ctx = ctx.clone();
                        let name = call.name.clone();
                        let args = call.parsed_args();
                        let handle = scope.spawn(move || {
                            let started = Instant::now();
                            let outcome = match registry.execute(&ctx, &name, &args) {
                                Ok(result) => ToolOutcome {
                                    content: result.content,
                                    is_error: result.is_error,
                                    images: result.images,
                                },
                                Err(e) => ToolOutcome {
                                    content: format!("tool '{}' failed: {}", name, e),
                                    is_error: true,
                                    images: Vec::new(),
                                },
                            };
                            (outcome, started.elapsed().as_millis() as u64)
                        });
                        (index, handle)
                    })
                    .collect();
                for (index, handle) in handles {
                    outcomes[index] = Some(handle.join().unwrap_or_else(|_| {
                        // A panicking tool is a bug, not a reason to lose the
                        // rest of the batch.
                        (
                            ToolOutcome {
                                content: "tool panicked; the run continues".to_string(),
                                is_error: true,
                                images: Vec::new(),
                            },
                            0,
                        )
                    }));
                }
            });

            for (index, call) in chunk.iter().enumerate() {
                let suppressed = skip[index].is_some();
                let (outcome, duration_ms) = match skip[index].take() {
                    Some(gated) => (gated, 0),
                    None => outcomes[index].take().unwrap_or_else(|| {
                        (
                            ToolOutcome {
                                content: "tool did not run".to_string(),
                                is_error: true,
                                images: Vec::new(),
                            },
                            0,
                        )
                    }),
                };
                sink.emit(AgentEvent::ToolFinished {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: outcome.content.clone(),
                    is_error: outcome.is_error,
                    duration_ms,
                });
                if !suppressed {
                    self.note_tool_outcome(call, &outcome, turn);
                    self.audit(
                        &call.name,
                        &call.parsed_args(),
                        if outcome.is_error {
                            "failed"
                        } else {
                            "allowed"
                        },
                        None,
                    );
                    let post = self.config.post_tool.clone();
                    if let Some(post) = post {
                        post(&call.name, &call.parsed_args(), outcome.is_error);
                    }
                }
                let content =
                    bound_tool_output(&call.name, &outcome.content, outcome.is_error, remaining);
                self.history
                    .push(tool_result_message(call, &outcome, content));
            }
            done += chunk.len();
            if let Some(reason) = self.policy_stop.take() {
                let reason = FinishReason::PolicyBlocked(reason);
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Some(reason);
            }
        }
        None
    }

    /// hook, approval and the audit trail mean the same thing in both. Gates
    /// are pure decisions; the parallel path runs them on the loop thread
    /// before spawning, so their order stays deterministic while the tools
    /// themselves still execute concurrently. Events are emitted by the
    /// caller, which keeps both paths' transcripts identical.
    fn gate_decision(&mut self, call: &ToolCall, args: &Json) -> Option<ToolOutcome> {
        // A rule written into the settings file comes before anything that
        // can approve. A mis-click must not be able to wave it through.
        if let Some(policy) = self.config.policy.clone() {
            if let Some(reason) = policy(&call.name, args) {
                let prior_blocks = self.blocked_repeat_count(&call.name, &reason);
                let repeated = prior_blocks + 1 >= BLOCKED_CALL_LIMIT;
                self.audit(
                    &call.name,
                    args,
                    if repeated { "blocked-again" } else { "blocked" },
                    Some(reason.clone()),
                );
                let content = if repeated {
                    format!(
                        "The operator's policy blocked this {} call again: {}. The run \
                         stops here. Report what you need and let the operator decide.",
                        call.name, reason
                    )
                } else {
                    format!(
                        "The operator's policy blocked this {} call: {}. Do not try to reach \
                         the same effect another way.",
                        call.name, reason
                    )
                };
                self.remember_blocked(&call.name, &reason);
                if repeated {
                    self.policy_stop = Some(reason);
                }
                return Some(ToolOutcome {
                    content,
                    is_error: true,
                    images: Vec::new(),
                });
            }
        }

        if let Some(pre) = self.config.pre_tool.clone() {
            if let Some(reason) = pre(&call.name, args) {
                self.audit(&call.name, args, "blocked", Some(reason.clone()));
                return Some(ToolOutcome {
                    content: reason,
                    is_error: true,
                    images: Vec::new(),
                });
            }
        }

        let table_approval = self
            .config
            .policy_approval
            .as_ref()
            .and_then(|decide| decide.approval(&call.name, args));
        if table_approval != Some(false) {
            let mut needs = match self.config.approval_scope {
                ApprovalScope::DestructiveOnly => self.registry.needs_approval(&call.name, args),
                ApprovalScope::AllMutating => self
                    .registry
                    .get(&call.name)
                    .map(|tool| tool.is_mutating())
                    .unwrap_or(false),
            };
            if table_approval == Some(true) {
                needs = true;
            }
            if needs {
                if let Some(approve) = &self.config.approve {
                    if !approve(&call.name, args) {
                        self.audit(&call.name, args, "declined", None);
                        let post = self.config.post_tool.clone();
                        if let Some(post) = post {
                            post(&call.name, args, true);
                        }
                        return Some(ToolOutcome {
                            content: format!(
                                "The operator declined this {} call. Do not attempt the same effect \
                                 through a different command; stop and ask the operator how to proceed.",
                                call.name
                            ),
                            is_error: true,
                            images: Vec::new(),
                        });
                    }
                } else if table_approval == Some(true) {
                    let reason = "the permissions table requires approval, but no approval channel is available";
                    self.audit(&call.name, args, "blocked", Some(reason.to_string()));
                    return Some(ToolOutcome {
                        content: format!("{}: {}", call.name, reason),
                        is_error: true,
                        images: Vec::new(),
                    });
                }
            }
        }
        None
    }

    /// Run one tool call and emit its events.
    fn execute_tool(&mut self, call: &ToolCall, sink: &mut dyn EventSink) -> ToolOutcome {
        sink.emit(AgentEvent::ToolRequested(call.clone()));
        let started = Instant::now();
        let args = call.parsed_args();

        if let Some(outcome) = self.gate_decision(call, &args) {
            sink.emit(AgentEvent::ToolFinished {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: outcome.content.clone(),
                is_error: outcome.is_error,
                duration_ms: started.elapsed().as_millis() as u64,
            });
            return outcome;
        }

        let outcome = match self.registry.execute(&self.ctx, &call.name, &args) {
            Ok(result) => ToolOutcome {
                content: result.content,
                is_error: result.is_error,
                images: result.images,
            },
            Err(e) => ToolOutcome {
                content: format!("tool '{}' failed: {}", call.name, e),
                is_error: true,
                images: Vec::new(),
            },
        };
        self.audit(
            &call.name,
            &args,
            if outcome.is_error {
                "failed"
            } else {
                "allowed"
            },
            None,
        );

        sink.emit(AgentEvent::ToolFinished {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: outcome.content.clone(),
            is_error: outcome.is_error,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        if let Some(post) = &self.config.post_tool {
            post(&call.name, &args, outcome.is_error);
        }
        outcome
    }

    /// Record one tool decision in the audit log, if one is attached.
    fn audit(&self, name: &str, args: &Json, decision: &str, reason: Option<String>) {
        if let Some(audit) = &self.config.audit {
            audit(name, args, decision, reason);
        }
    }

    /// How often this tool was refused for the same reason in this run.
    ///
    /// The window matches the repeat guard: a refusal from twenty turns ago
    /// says nothing about the model now. Arguments are not compared — a
    /// rewritten command that earns the same refusal is the same dead end.
    fn blocked_repeat_count(&self, name: &str, reason: &str) -> usize {
        let key = blocked_key(name, reason);
        self.blocked_calls
            .iter()
            .filter(|blocked| {
                blocked.key == key
                    && self.current_turn.saturating_sub(blocked.turn) <= REPEAT_GUARD_WINDOW
            })
            .count()
    }

    /// Remember a refused call.
    fn remember_blocked(&mut self, name: &str, reason: &str) {
        if self.blocked_calls.len() >= REPEAT_GUARD_MAX_ENTRIES {
            self.blocked_calls.remove(0);
        }
        self.blocked_calls.push(BlockedCall {
            name: name.to_string(),
            key: blocked_key(name, reason),
            turn: self.current_turn,
            reason: reason.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoProvider;

    impl argent_ai::Provider for NoProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn models(&self) -> argent_core::error::Result<Vec<argent_ai::ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &argent_ai::ChatRequest,
        ) -> argent_core::error::Result<
            Box<dyn Iterator<Item = argent_core::error::Result<argent_ai::StreamEvent>> + Send>,
        > {
            unreachable!("the gate test never calls the provider")
        }
    }

    fn agent(approval: Option<bool>) -> Agent {
        let mut config = AgentConfig::new("test");
        config.policy_approval = approval.map(|value| {
            std::sync::Arc::new(move |_: &str, _: &Json| Some(value))
                as std::sync::Arc<dyn crate::config::PolicyApproval>
        });
        Agent::new(
            Box::new(NoProvider),
            std::sync::Arc::new(ToolRegistry::with_core()),
            config,
            ToolCtx::new(std::env::temp_dir()),
            "system",
        )
    }

    #[test]
    fn an_ask_rule_blocks_without_an_approval_channel() {
        let mut agent = agent(Some(true));
        let call = argent_ai::ToolCall::new("c1", "bash", r#"{"command":"echo hi"}"#);
        let outcome = agent
            .gate_decision(&call, &call.parsed_args())
            .expect("ask must block");
        assert!(outcome.is_error);
        assert!(outcome.content.contains("no approval channel"));
    }

    #[test]
    fn an_allow_rule_skips_approval() {
        let mut agent = agent(Some(false));
        let call = argent_ai::ToolCall::new("c1", "bash", r#"{"command":"echo hi"}"#);
        assert!(agent.gate_decision(&call, &call.parsed_args()).is_none());
    }
}
