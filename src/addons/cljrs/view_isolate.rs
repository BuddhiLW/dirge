//! Adapter: the cljrs `dirge.view` reducer behind the view seam's
//! [`Reducer`] port. It gets a runtime of its own on a thread of its own
//! (the view seam's [`ThreadEngine`]), apart from the addon isolate: an
//! addon hook running during an agent turn never delays a view change,
//! and a view change never queues behind one.

use std::cell::RefCell;
use std::rc::Rc;

use cljrs_runtime::tiered::Env;
use cljrs_runtime::{ExecutionMode, Runtime};
use serde_json::Value as Json;

use super::isolate::{ISOLATE_STACK_BYTES, eval_source, install_args};
use crate::ui::view::engine::ThreadEngine;
use crate::ui::view::port::{Reducer, UpdateSink};
use crate::ui::view::{ViewEvent, ViewModel, ViewUpdate, wire};

/// The reducer's namespace, embedded in the binary.
const VIEW_NS: &str = "dirge.view";
const VIEW_SRC: &str = include_str!("view.cljc");
/// The external panels' policy, required by `dirge.view`.
const PANELS_NS: &str = "dirge.panels";
const PANELS_SRC: &str = include_str!("panels.cljc");

/// Private namespace through which an event reaches Clojure.
const ARGS_NS: &str = "dirge.view.bridge";

/// `dirge.view` in a runtime of its own. Not `Send`: built on the
/// engine thread.
pub struct CljrsReducer {
    env: Env,
    inbox: Rc<RefCell<Vec<Json>>>,
}

impl CljrsReducer {
    pub fn boot() -> Result<Self, String> {
        let runtime = Runtime::builder()
            .execution_mode(ExecutionMode::Tiered)
            .builtin_source(PANELS_NS, PANELS_SRC)
            .builtin_source(VIEW_NS, VIEW_SRC)
            .build()
            .map_err(|e| format!("cannot build the view runtime: {e}"))?;
        cljrs_stdlib::install(&runtime);
        let inbox = Rc::new(RefCell::new(Vec::new()));
        install_args(runtime.globals(), ARGS_NS, inbox.clone());
        let mut reducer = Self {
            env: runtime.env("user"),
            inbox,
        };
        eval_source(&mut reducer.env, &format!("(require '{VIEW_NS})"))
            .map_err(|e| format!("cannot load {VIEW_NS}: {e}"))?;
        Ok(reducer)
    }
}

impl Reducer for CljrsReducer {
    fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
        *self.inbox.borrow_mut() = vec![wire::encode_event(event)];
        let answer = eval_source(
            &mut self.env,
            &format!("(apply {VIEW_NS}/dispatch! ({ARGS_NS}/args))"),
        );
        self.inbox.borrow_mut().clear();
        wire::decode_update(&answer?)
    }
}

/// The cljrs view engine, for the view seam's engine table.
pub fn engine(sink: UpdateSink) -> Result<(ThreadEngine, ViewModel), String> {
    ThreadEngine::spawn(
        "dirge-view-cljrs",
        ISOLATE_STACK_BYTES,
        CljrsReducer::boot,
        sink,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::domain::{NoticeLevel, PaintSpan, ViewEffect};
    use crate::ui::view::native::NativeReducer;
    use crate::ui::view::tests::parity_script;

    /// Run `f` on a thread with the isolate's stack: the tree-walking
    /// evaluator recurses deeper than a test thread's default.
    fn on_isolate_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(ISOLATE_STACK_BYTES)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn cljrs_and_native_answer_every_event_alike() {
        on_isolate_stack(|| {
            let mut cljrs = CljrsReducer::boot().unwrap();
            let mut native = NativeReducer::default();
            for event in parity_script() {
                let want = native.step(&event).unwrap();
                let mut got = cljrs.step(&event).unwrap();
                // Only the cljrs view owns the panels; the rest agrees.
                assert!(got.model.owns_feed);
                got.model.owns_feed = false;
                assert_eq!(got, want, "diverged on {event:?}");
            }
        });
    }

    #[test]
    fn unknown_event_types_are_refused_by_the_reducer() {
        on_isolate_stack(|| {
            let mut cljrs = CljrsReducer::boot().unwrap();
            *cljrs.inbox.borrow_mut() = vec![serde_json::json!({"type": "teleport"})];
            let answer = eval_source(
                &mut cljrs.env,
                &format!("(apply {VIEW_NS}/dispatch! ({ARGS_NS}/args))"),
            )
            .unwrap();
            let update = wire::decode_update(&answer).unwrap();
            assert_eq!(update.effects.len(), 1);
            assert!(format!("{:?}", update.effects[0]).contains("unknown view event: teleport"));
        });
    }

    fn feed(op: Json) -> ViewEvent {
        ViewEvent::Feed { op }
    }

    fn span(text: &str, face: &str) -> PaintSpan {
        PaintSpan {
            text: text.into(),
            face: face.into(),
        }
    }

    /// The panels' policy runs in cljrs: feed ops in, paint effects out.
    #[test]
    fn feed_ops_become_paint_effects() {
        use serde_json::json;
        on_isolate_stack(|| {
            let mut cljrs = CljrsReducer::boot().unwrap();
            let u = cljrs.step(&ViewEvent::Init).unwrap();
            assert!(u.model.owns_feed, "the cljrs view owns the panels");

            let u = cljrs
                .step(&feed(json!({
                    "op": "ui/show-panel", "panel/id": "cf",
                    "doc": {"doc/title": "Flow"},
                    "lines": [
                        {"text": "Flow", "face": "title"},
                        {"text": "", "face": "plain"},
                        {"face": "dim", "spans": [{"text": "+ ", "face": "added"}, "a\nb"]},
                        "plain"
                    ]
                })))
                .unwrap();
            assert_eq!(
                u.effects,
                vec![ViewEffect::Paint {
                    id: "cf".into(),
                    title: "Flow".into(),
                    rows: vec![
                        vec![span("+ ", "added"), span("a", "dim")],
                        vec![span("b", "dim")],
                        vec![span("plain", "")],
                    ],
                    tail: false,
                    offset: 0,
                    focus: false,
                }]
            );

            let u = cljrs
                .step(&feed(
                    json!({"op": "ui/focus-tab", "panel/id": "log", "title": "Log"}),
                ))
                .unwrap();
            assert!(matches!(
                &u.effects[..],
                [ViewEffect::Paint { id, tail: true, focus: true, .. }] if id == "log"
            ));
            let u = cljrs
                .step(&feed(json!({"op": "ui/append-tab", "panel/id": "log",
                                   "line": {"text": "x", "face": "warn"}})))
                .unwrap();
            assert!(matches!(
                &u.effects[..],
                [ViewEffect::Paint { rows, focus: true, .. }] if rows == &vec![vec![span("x", "warn")]]
            ));

            let u = cljrs
                .step(&feed(
                    json!({"op": "ui/notify", "message": "hi", "level": "warning"}),
                ))
                .unwrap();
            assert_eq!(u.effects, vec![ViewEffect::notify(NoticeLevel::Warn, "hi")]);
            assert!(
                cljrs
                    .step(&feed(json!({"op": "ui/teleport"})))
                    .unwrap()
                    .effects
                    .is_empty(),
                "unknown ops are ignored"
            );

            let u = cljrs.step(&feed(json!({"op": "feed/ended"}))).unwrap();
            assert_eq!(
                u.effects,
                vec![
                    ViewEffect::Unpaint { id: "cf".into() },
                    ViewEffect::Unpaint { id: "log".into() },
                ]
            );
        });
    }

    #[test]
    fn panels_are_bounded_evicting_the_oldest() {
        use serde_json::json;
        on_isolate_stack(|| {
            let mut cljrs = CljrsReducer::boot().unwrap();
            for i in 0..16 {
                cljrs
                    .step(&feed(
                        json!({"op": "ui/show-panel", "panel/id": format!("p{i}")}),
                    ))
                    .unwrap();
            }
            let u = cljrs
                .step(&feed(json!({"op": "ui/show-panel", "panel/id": "new"})))
                .unwrap();
            assert_eq!(u.effects[0], ViewEffect::Unpaint { id: "p0".into() });
            assert!(matches!(&u.effects[1], ViewEffect::Paint { id, .. } if id == "new"));
        });
    }

    #[test]
    fn the_engine_boots_and_answers_off_thread() {
        let (sink, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (engine, model) = engine(sink).unwrap();
        assert!(model.owns_command("swarm"));
        use crate::ui::view::port::ViewEngine;
        engine.submit(ViewEvent::command("swarm", &["on"]));
        assert!(rx.blocking_recv().unwrap().model.swarm_open());
    }
}
