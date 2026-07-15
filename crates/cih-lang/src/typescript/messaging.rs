//! Messaging / realtime detection — kafkajs, Bull/BullMQ, and socket.io
//! publish/subscribe sites become event-contract (publish/listen) sites.

use cih_core::{
    file_id, MessagingFramework, NodeId,
};
use tree_sitter::Node as TsNode;


use super::builder::Builder;
use super::helpers::*;

// ── Messaging / realtime (P5) ─────────────────────────────────────────────────

/// True if `value` is `new Queue('name')` (Bull/BullMQ).
pub(super) fn is_new_queue(value: TsNode<'_>, src: &str) -> bool {
    value.kind() == "new_expression"
        && value
            .child_by_field_name("constructor")
            .map(|c| text(c, src))
            .as_deref()
            == Some("Queue")
}

/// Pre-pass: record `const q = new Queue('emails')` vars → queue name.
pub(super) fn collect_queue_instances(root: TsNode<'_>, src: &str, builder: &mut Builder) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "variable_declarator" {
            if let (Some(name), Some(value)) = (
                n.child_by_field_name("name"),
                n.child_by_field_name("value"),
            ) {
                if name.kind() == "identifier" && is_new_queue(value, src) {
                    if let Some(q) = first_string_arg_in_call(value, src) {
                        builder.queue_instances.insert(text(name, src), q);
                    }
                }
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Topic literal from a kafkajs `{ topic: 't', … }` first-arg config.
pub(super) fn kafka_topic_arg(node: TsNode<'_>, src: &str) -> Option<String> {
    let arg0 = ts_positional_argument(node, 0)?;
    if arg0.kind() != "object" {
        return None;
    }
    literal_ts_string(object_pair_value(arg0, "topic", src)?, src)
}

/// Detect a messaging call and emit an `EventPublish`/`EventListen` contract:
/// socket.io, kafkajs, Bull/BullMQ, amqplib (all import-gated to bound false
/// positives on the generic method names `emit`/`on`/`send`/`add`).
pub(super) fn try_emit_messaging(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "member_expression" {
        return;
    }
    let (Some(obj), Some(prop_node)) = (
        func.child_by_field_name("object"),
        func.child_by_field_name("property"),
    ) else {
        return;
    };
    let obj_text = text(obj, src);
    let prop = text(prop_node, src);
    let in_callable = || {
        enclosing_fn
            .cloned()
            .unwrap_or_else(|| file_id(&builder.rel))
    };

    // socket.io realtime events.
    if builder.imports_pkg("socket.io") || builder.imports_pkg("socket.io-client") {
        let publish = match prop.as_str() {
            "emit" => Some(true),
            "on" => Some(false),
            _ => None,
        };
        if let Some(is_pub) = publish {
            if let Some(topic) = first_string_arg_in_call(node, src) {
                builder.emit_event_contract(
                    node,
                    topic,
                    MessagingFramework::SocketIo,
                    is_pub,
                    in_callable(),
                );
                return;
            }
        }
    }

    // kafkajs producer/consumer.
    if builder.imports_pkg("kafkajs") {
        let publish = match prop.as_str() {
            "send" => Some(true),
            "subscribe" => Some(false),
            _ => None,
        };
        if let Some(is_pub) = publish {
            if let Some(topic) = kafka_topic_arg(node, src) {
                builder.emit_event_contract(
                    node,
                    topic,
                    MessagingFramework::Kafka,
                    is_pub,
                    in_callable(),
                );
                return;
            }
        }
    }

    // Bull/BullMQ: `queue.add(...)` publishes to the tracked queue name.
    if (builder.imports_pkg("bull") || builder.imports_pkg("bullmq")) && prop == "add" {
        if let Some(topic) = builder.queue_instances.get(&obj_text).cloned() {
            builder.emit_event_contract(
                node,
                topic,
                MessagingFramework::Bull,
                true,
                in_callable(),
            );
            return;
        }
    }

    // amqplib (RabbitMQ) channel ops.
    if builder.imports_pkg("amqplib") {
        let publish = match prop.as_str() {
            "sendToQueue" | "publish" => Some(true),
            "consume" => Some(false),
            _ => None,
        };
        if let Some(is_pub) = publish {
            if let Some(topic) = first_string_arg_in_call(node, src) {
                builder.emit_event_contract(
                    node,
                    topic,
                    MessagingFramework::Rabbitmq,
                    is_pub,
                    in_callable(),
                );
            }
        }
    }
}

