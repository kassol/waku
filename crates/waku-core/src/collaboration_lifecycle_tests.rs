//! One public task mixes independent work, nested decisions and final results.
use super::*;

#[test]
fn collaboration_lifecycle_socket_two_children_nested_decisions_and_results_have_exact_counts() {
    for result_first in [false, true] {
        let (root, parent, manager) = seed_queue();
        let mut other = manager.clone();
        other.id = Uuid::new_v4();
        other.messages.clear();
        other.turns.clear();
        other.begin_turn("Independent native work");
        other.finish_active_turn(crate::model::TurnStatus::Completed);
        let mut leaf = other.clone();
        leaf.id = Uuid::new_v4();
        leaf.parent_session_id = Some(manager.id);
        let store = StateStore::daemon(root.join("state.db"));
        let mut state = store.load().unwrap();
        state.sessions.extend([other.clone(), leaf.clone()]);
        state.mark_session_dirty(other.id);
        state.mark_session_dirty(leaf.id);
        store.save(&mut state).unwrap();
        let server = QueueServer::open(&root);
        let client = server.connect();
        let (release, gate) = crossbeam_channel::unbounded();
        for _ in 0..8 {
            release.send(()).unwrap();
        }
        *server.backend.callback_gate.lock() = Some(gate);
        let pr = server.start(&client, &root, parent.id);
        let mr = server.start(&client, &root, manager.id);
        let br = server.start(&client, &root, other.id);
        let lr = server.start(&client, &root, leaf.id);
        for (id, runtime, prompt) in [
            (parent.id, pr, "Delegate both tasks within this goal"),
            (manager.id, mr, "Prepare managed result"),
            (other.id, br, "Independent native work"),
            (leaf.id, lr, "Prepare optional report"),
        ] {
            client
                .request(
                    id,
                    runtime,
                    Command::Prompt {
                        prompt: prompt.into(),
                        turn_id: None,
                        message_id: None,
                    },
                )
                .unwrap();
        }
        client
            .request(
                parent.id,
                pr,
                Command::StewardWait {
                    session_ids: vec![manager.id, other.id],
                },
            )
            .unwrap();
        let operation = |value| {
            serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":value}))
                .unwrap()
        };
        let ordinary = Uuid::new_v4();
        let requested=client.request(manager.id,mr,operation(json!({"type":"request","request_id":ordinary,"question":"Which format?","context":"Original goal","recommendation":"JSON","blocked_work":"Output"}))).unwrap();
        let authority =
            serde_json::to_value(requested).unwrap()["requests"][0]["instruction_message_id"]
                .clone();
        server.finish(manager.id);
        let sink = server.backend.sinks.lock().get(&other.id).unwrap().clone();
        sink.send(
            event_to_wire(DriverEvent::Permission {
                request_id: "combined-native".into(),
                title: "Write output".into(),
                detail: "Temporary output only".into(),
                options: vec![crate::model::PermissionOption {
                    id: "allow".into(),
                    label: "Allow once".into(),
                    allow: true,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        for _ in 0..12 {
            sink.send(event_to_wire(DriverEvent::TextDelta("ordinary progress".into())).unwrap())
                .unwrap();
        }
        assert!(
            server
                .calls
                .recv_timeout(Duration::from_millis(150))
                .is_err(),
            "busy main and ordinary progress must not start another model turn"
        );
        server.finish(parent.id);
        let first = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(first.0, parent.id);
        assert!(first.1.contains("automatic decision notification"));
        let decide = operation(
            json!({"type":"decide","session_id":manager.id,"request_id":ordinary,"decision":"Use JSON","authority_message_id":authority}),
        );
        client.request(parent.id, pr, decide.clone()).unwrap();
        let ordinary_call = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(ordinary_call.0, manager.id);
        assert!(ordinary_call.1.contains("Use JSON"));
        client.request(parent.id, pr, decide).unwrap();
        let ResponsePayload::StewardDecisions { requests } = client
            .request(
                parent.id,
                pr,
                operation(json!({"type":"list","session_id":other.id})),
            )
            .unwrap()
        else {
            panic!("native list")
        };
        let native = operation(
            json!({"type":"decideNative","session_id":other.id,"request_id":requests[0].id,"authority_message_id":authority,"response":{"type":"permission","option_id":"allow"}}),
        );
        client.request(parent.id, pr, native.clone()).unwrap();
        let native_call = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(native_call.0, other.id);
        assert!(native_call.1.starts_with("native:"));
        client.request(parent.id, pr, native).unwrap();
        client
            .request(
                manager.id,
                mr,
                Command::StewardWait {
                    session_ids: vec![leaf.id],
                },
            )
            .unwrap();
        let question = Uuid::new_v4();
        client.request(leaf.id,lr,operation(json!({"type":"request","request_id":question,"question":"Add report?","context":"Outside original scope","recommendation":"Add","blocked_work":"Report"}))).unwrap();
        let escalate = |child, id| {
            operation(
                json!({"type":"escalate","session_id":child,"request_id":id,"reason":"Needs explicit user","options":[{"label":"Add","impact":"Extra output"}],"impact":"Extra output"}),
            )
        };
        let forwarded = client
            .request(manager.id, mr, escalate(leaf.id, question))
            .unwrap();
        let upstream: Uuid = serde_json::from_value(
            serde_json::to_value(forwarded).unwrap()["requests"][0]["upstream_request_id"].clone(),
        )
        .unwrap();
        client
            .request(parent.id, pr, escalate(manager.id, upstream))
            .unwrap();
        server.finish(leaf.id);
        server.finish(manager.id);
        if result_first {
            server.finish(other.id);
        }
        let answer = Command::AnswerDecision {
            child_session_id: manager.id,
            request_id: upstream,
            answer: "Add the explicit report".into(),
        };
        client.request(parent.id, pr, answer.clone()).unwrap();
        let leaf_call = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(leaf_call.0, leaf.id);
        assert!(leaf_call.1.contains("Add the explicit report"));
        client.request(parent.id, pr, answer).unwrap();
        if !result_first {
            server.finish(other.id);
        }
        assert!(
            server
                .calls
                .recv_timeout(Duration::from_millis(150))
                .is_err(),
            "result and answer must wait for the busy main turn"
        );
        server.finish(parent.id);
        let other_result = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(other_result.0, parent.id);
        assert!(
            other_result
                .1
                .contains("automatic child-session notification")
        );
        // A result callback consumes its wait. The public instruction explicitly
        // requires the manager to register the remaining work before ending its turn.
        assert!(other_result.1.contains("call waku_wait"));
        let ResponsePayload::StewardWait {
            wait: Some(remaining),
            ..
        } = client
            .request(
                parent.id,
                pr,
                Command::StewardWait {
                    session_ids: vec![manager.id],
                },
            )
            .unwrap()
        else {
            panic!("remaining result wait")
        };
        assert_eq!(remaining.targets.len(), 1);
        assert_eq!(remaining.targets[0].session_id, manager.id);
        server.finish(leaf.id);
        let manager_result = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(manager_result.0, manager.id);
        assert!(
            manager_result
                .1
                .contains("automatic child-session notification")
        );
        server.finish(manager.id);
        assert!(
            server
                .calls
                .recv_timeout(Duration::from_millis(150))
                .is_err()
        );
        server.finish(parent.id);
        let final_result = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(final_result.0, parent.id);
        assert!(
            final_result
                .1
                .contains("automatic child-session notification")
        );
        server.finish(parent.id);
        assert!(
            server
                .calls
                .recv_timeout(Duration::from_millis(150))
                .is_err(),
            "exactly three main callbacks and one nested result callback"
        );
        // Each saved fixed result adds one summary, even when completion is repeated.
        for (owner, target) in [
            (manager.id, leaf.id),
            (parent.id, manager.id),
            (parent.id, other.id),
        ] {
            let command = lifecycle_complete_command(&client, owner, target, "accepted");
            client.request(owner, Uuid::nil(), command.clone()).unwrap();
            client.request(owner, Uuid::nil(), command).unwrap();
        }
        let ResponsePayload::Session {
            session: Some(main),
        } = client
            .request(
                parent.id,
                Uuid::nil(),
                Command::HydrateSession {
                    session_id: parent.id,
                },
            )
            .unwrap()
        else {
            panic!("main")
        };
        assert_eq!(
            main.messages
                .iter()
                .filter(|m| m.content.contains("Explicit completion"))
                .count(),
            2
        );
        let ResponsePayload::Session {
            session: Some(nested),
        } = client
            .request(
                manager.id,
                Uuid::nil(),
                Command::HydrateSession {
                    session_id: manager.id,
                },
            )
            .unwrap()
        else {
            panic!("nested")
        };
        assert_eq!(
            nested
                .messages
                .iter()
                .filter(|m| m.content.contains("Explicit completion"))
                .count(),
            1
        );
        assert!(nested.archived);
        assert_eq!(nested.completions.len(), 1);
        drop(client);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}
