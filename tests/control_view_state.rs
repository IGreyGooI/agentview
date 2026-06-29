use agentview::prelude::*;
use serde_json::json;

#[test]
fn full_update_wraps_snapshot_with_turn_prompt() {
    let turn_prompt = json!({
        "instructions": "submit a legal UCI move",
        "schema": {
            "type": "object",
            "required": ["uci"]
        }
    });
    let snapshot = ViewSnapshot::new(12, "turn-1", "fen board", turn_prompt.clone());

    assert_eq!(snapshot.view_epoch, 12);
    assert_eq!(snapshot.turn_id, "turn-1");
    assert_eq!(snapshot.turn_prompt, turn_prompt);

    let update = ViewUpdate::full(10, snapshot.clone());

    assert_eq!(update.base_epoch, 10);
    assert_eq!(update.view_epoch, 12);
    assert_eq!(update.snapshot(), Some(&snapshot));
}

#[test]
fn partial_update_carries_patch_and_next_epoch() {
    let patch = ViewPatch::json(json!([
        { "op": "replace", "path": "/side_to_move", "value": "black" }
    ]));
    let update: ViewUpdate<&'static str> = ViewUpdate::partial(12, 13, patch.clone());

    assert_eq!(update.base_epoch, 12);
    assert_eq!(update.view_epoch, 13);
    assert_eq!(update.patch(), Some(&patch));
}

#[derive(Default)]
struct CollectTurnSink {
    replies: Vec<ControlReply>,
}

#[async_trait::async_trait]
impl TurnSink<ControlReply> for CollectTurnSink {
    type Output = Vec<ControlReply>;

    async fn on_event(&mut self, reply: ControlReply) {
        self.replies.push(reply);
    }

    async fn finish(self: Box<Self>) -> Self::Output {
        self.replies
    }
}

#[tokio::test]
async fn turn_sink_receives_external_control_reply() {
    let mut sink = CollectTurnSink::default();

    sink.on_event(ControlReply::structured(json!({ "uci": "e2e4" })))
        .await;
    let replies = Box::new(sink).finish().await;

    assert_eq!(replies[0].as_structured().unwrap()["uci"], "e2e4");
}
