//! The parent side of the framed sign-in ceremony, under test.
//!
//! FOR: [`read_framed_message`] and [`read_embed_size`] are where data the app
//! did not produce first reaches it. Anything on the page can call
//! `postMessage`, and a frame the page mounted can post whatever it likes; what
//! decides whether one of those messages starts a token exchange is the pair of
//! checks in those two readers. Until this file they had no test at all — the
//! e2e suite drives a real idp.to ceremony, so it exercises the accepting path
//! and none of the refusing ones.
//!
//! Everything here is a plain unit test: a `MessageEvent` built in the page,
//! read, and thrown away. `MessageEventInit` lets a synthetic event carry any
//! `origin` string, which is exactly what makes the origin check testable —
//! the readers compare against a compile-time constant, and the tests hand them
//! both the constant and other values. No network, no idp.to, no frame.
//!
//! A browser is required (`wasm-pack test --headless --chrome`): `MessageEvent`
//! and `encodeURIComponent` are the browser's, not node's.
//!
//! The module is a child of `auth`, so it reads that module's private items
//! without any of them being widened for it — [`message_field`] and
//! [`object_field`] are tested directly, at the level where their rules are
//! written, rather than inferred through a caller.

use super::*;
use wasm_bindgen_test::*;
use web_sys::MessageEventInit;

wasm_bindgen_test_configure!(run_in_browser);

/// An origin that is neither idp.to's property host nor our own.
const OTHER_ORIGIN: &str = "https://elsewhere.example";

/// A message as the browser would deliver it: `data` under a chosen `origin`.
/// `event.source` is deliberately left null — the readers never consult it, and
/// a test that set it would imply they did.
fn message_from(origin: &str, data: &JsValue) -> MessageEvent {
    let init = MessageEventInit::new();
    init.set_origin(origin);
    init.set_data(data);
    MessageEvent::new_with_event_init_dict("message", &init).expect("construct a MessageEvent")
}

/// A JS object with the given members, in order.
fn obj(members: &[(&str, JsValue)]) -> JsValue {
    let object = js_sys::Object::new();
    for (name, value) in members {
        js_sys::Reflect::set(&object, &JsValue::from_str(name), value).expect("set a member");
    }
    object.into()
}

/// idp.to's `web_message` envelope around one OAuth response.
fn authorization_response(response: JsValue) -> JsValue {
    obj(&[("type", JsValue::from_str(AUTHORIZATION_RESPONSE_TYPE)), ("response", response)])
}

/// The envelope a successful framed attempt produces.
fn success_envelope(code: &str, state: &str) -> JsValue {
    authorization_response(obj(&[("code", JsValue::from_str(code)), ("state", JsValue::from_str(state))]))
}

/// The attempt a test is waiting on.
fn attempt() -> Option<String> { Some("the-attempts-state".to_string()) }

// --- read_framed_message: the accepting path ---------------------------------

#[wasm_bindgen_test]
fn the_attempts_own_response_is_accepted_and_its_state_is_spent() {
    let mut expected = attempt();
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &success_envelope("the-code", "the-attempts-state"));

    match read_framed_message(&event, &mut expected) {
        FramedMessage::Accepted { code } => assert_eq!(code, "the-code"),
        _ => panic!("the attempt's own response must be accepted"),
    }
    assert_eq!(expected, None, "accepting must spend the state");
}

#[wasm_bindgen_test]
fn a_second_copy_of_the_accepted_response_is_ignored() {
    let mut expected = attempt();
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &success_envelope("the-code", "the-attempts-state"));

    read_framed_message(&event, &mut expected);
    // Byte-identical to the one just taken. The spent state is the whole
    // defence: a code is single-use at idp.to, and a second exchange of it
    // would fail there, but the ceremony must not start one.
    assert!(matches!(read_framed_message(&event, &mut expected), FramedMessage::Ignored));
}

// --- read_framed_message: what it refuses ------------------------------------

#[wasm_bindgen_test]
fn a_response_from_any_other_origin_is_ignored() {
    let envelope = success_envelope("the-code", "the-attempts-state");
    let own_origin = window().unwrap().location().origin().unwrap();

    // Our own origin is on this list on purpose. A `web_message` result is the
    // framed document speaking, and that document is idp.to's; a message the
    // page posted to itself is not the frame, however well-formed it looks.
    for origin in [OTHER_ORIGIN, own_origin.as_str(), "https://login.idp.to", "https://ankurah.login.idp.to.example"] {
        let mut expected = attempt();
        let event = message_from(origin, &envelope);
        assert!(matches!(read_framed_message(&event, &mut expected), FramedMessage::Ignored), "origin {origin} was not refused");
        assert_eq!(expected, attempt(), "a refused origin must not spend the state");
    }
}

#[wasm_bindgen_test]
fn an_envelope_of_another_type_is_ignored() {
    let response = obj(&[("code", JsValue::from_str("the-code")), ("state", JsValue::from_str("the-attempts-state"))]);
    for envelope_type in [EMBED_SIZE_TYPE, CALLBACK_MESSAGE_TYPE, "", "AUTHORIZATION_RESPONSE"] {
        let mut expected = attempt();
        let data = obj(&[("type", JsValue::from_str(envelope_type)), ("response", response.clone())]);
        let event = message_from(FRAMED_MESSAGE_ORIGIN, &data);
        assert!(matches!(read_framed_message(&event, &mut expected), FramedMessage::Ignored), "type {envelope_type:?} was not refused");
        assert_eq!(expected, attempt());
    }
}

#[wasm_bindgen_test]
fn an_envelope_with_no_response_object_is_ignored_without_spending_the_state() {
    // An array reaches the state check rather than stopping at `object_field`
    // — `typeof [] === "object"` — and is refused there for carrying no
    // `state`. The outcome is the same; the route is worth knowing.
    let cases: [(&str, JsValue); 5] = [
        ("absent", JsValue::UNDEFINED),
        ("null", JsValue::NULL),
        ("a string", JsValue::from_str("code=the-code&state=the-attempts-state")),
        ("a number", JsValue::from_f64(42.0)),
        ("an array", js_sys::Array::of2(&JsValue::from_str("the-code"), &JsValue::from_str("the-attempts-state")).into()),
    ];
    for (shape, response) in cases {
        let mut expected = attempt();
        let event = message_from(FRAMED_MESSAGE_ORIGIN, &authorization_response(response));
        assert!(matches!(read_framed_message(&event, &mut expected), FramedMessage::Ignored), "a `response` that was {shape} was not refused");
        assert_eq!(expected, attempt(), "a `response` that was {shape} spent the state");
    }
}

#[wasm_bindgen_test]
fn a_response_naming_another_attempts_state_is_ignored_without_spending_ours() {
    for state in ["another-attempts-state", "", "the-attempts-stat", "the-attempts-state "] {
        let mut expected = attempt();
        let event = message_from(FRAMED_MESSAGE_ORIGIN, &success_envelope("the-code", state));
        assert!(matches!(read_framed_message(&event, &mut expected), FramedMessage::Ignored), "state {state:?} was not refused");
        assert_eq!(expected, attempt(), "state {state:?} spent our state");
    }
}

#[wasm_bindgen_test]
fn a_response_with_no_state_member_at_all_is_ignored() {
    let mut expected = attempt();
    let envelope = authorization_response(obj(&[("code", JsValue::from_str("the-code"))]));
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &envelope);
    assert!(matches!(read_framed_message(&event, &mut expected), FramedMessage::Ignored));
    assert_eq!(expected, attempt());
}

#[wasm_bindgen_test]
fn a_response_arriving_after_the_ceremony_settled_is_ignored() {
    // What the ceremony holds once it has taken a result, or before it has
    // started one. A well-formed envelope has nothing to match against.
    let mut settled: Option<String> = None;
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &success_envelope("the-code", "the-attempts-state"));
    assert!(matches!(read_framed_message(&event, &mut settled), FramedMessage::Ignored));
}

// --- read_framed_message: the failing paths ----------------------------------

#[wasm_bindgen_test]
fn an_error_response_is_reported_in_idp_tos_words_and_spends_the_state() {
    let mut expected = attempt();
    let envelope = authorization_response(obj(&[
        ("error", JsValue::from_str("access_denied")),
        ("error_description", JsValue::from_str("the person declined")),
        ("state", JsValue::from_str("the-attempts-state")),
    ]));
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &envelope);

    match read_framed_message(&event, &mut expected) {
        FramedMessage::Failed(message) => {
            assert!(message.contains("access_denied"), "the refusal must name idp.to's error: {message}");
            assert!(message.contains("the person declined"), "the refusal must carry idp.to's description: {message}");
        }
        _ => panic!("an error response must fail the attempt"),
    }
    assert_eq!(expected, None, "a refusal settles the attempt, so its state is spent too");
}

#[wasm_bindgen_test]
fn an_invalid_scope_refusal_is_worded_as_retry_later() {
    // idp.to answers `invalid_scope` when role configuration for this
    // application has not finished activating. Our server requires the roles
    // claim, so there is nothing to degrade to — the card says "try again
    // shortly" rather than quoting the raw code at the reader.
    let mut expected = attempt();
    let envelope = authorization_response(obj(&[
        ("error", JsValue::from_str("invalid_scope")),
        ("state", JsValue::from_str("the-attempts-state")),
    ]));
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &envelope);

    match read_framed_message(&event, &mut expected) {
        FramedMessage::Failed(message) => assert!(message.contains("try signing in again shortly"), "unexpected wording: {message}"),
        _ => panic!("an error response must fail the attempt"),
    }
}

#[wasm_bindgen_test]
fn a_matching_response_carrying_neither_code_nor_error_fails_and_spends_the_state() {
    // The state matched, so this IS our attempt's result — it just says
    // nothing. Reporting it as Ignored would leave the ceremony spinning on a
    // frame that has already answered.
    let mut expected = attempt();
    let envelope = authorization_response(obj(&[("state", JsValue::from_str("the-attempts-state"))]));
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &envelope);

    match read_framed_message(&event, &mut expected) {
        FramedMessage::Failed(message) => assert!(message.contains("neither a code nor an error"), "unexpected wording: {message}"),
        _ => panic!("an empty result must fail the attempt, not be ignored"),
    }
    assert_eq!(expected, None);
}

#[wasm_bindgen_test]
fn an_empty_code_is_a_failure_rather_than_an_accepted_exchange() {
    // `message_field` treats absent and empty alike, so an empty `code` is no
    // code. What matters is that it does not reach `complete_sign_in`.
    let mut expected = attempt();
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &success_envelope("", "the-attempts-state"));
    assert!(matches!(read_framed_message(&event, &mut expected), FramedMessage::Failed(_)));
    assert_eq!(expected, None);
}

// --- read_embed_size ----------------------------------------------------------

/// idp.to's frame-size report, `{type: "idp-embed-size", height}`.
fn size_report(height: JsValue) -> JsValue { obj(&[("type", JsValue::from_str(EMBED_SIZE_TYPE)), ("height", height)]) }

#[wasm_bindgen_test]
fn a_size_report_from_the_property_host_yields_its_height() {
    let event = message_from(FRAMED_MESSAGE_ORIGIN, &size_report(JsValue::from_f64(480.5)));
    assert_eq!(read_embed_size(&event), Some(480.5));
}

#[wasm_bindgen_test]
fn a_size_report_from_any_other_origin_is_refused() {
    let own_origin = window().unwrap().location().origin().unwrap();
    for origin in [OTHER_ORIGIN, own_origin.as_str(), "https://ankurah.login.idp.to.example"] {
        let event = message_from(origin, &size_report(JsValue::from_f64(480.0)));
        assert_eq!(read_embed_size(&event), None, "origin {origin} was not refused");
    }
}

#[wasm_bindgen_test]
fn a_height_on_a_message_of_another_type_is_not_a_size_report() {
    for envelope_type in [AUTHORIZATION_RESPONSE_TYPE, CALLBACK_MESSAGE_TYPE, "", "idp-embed-size "] {
        let data = obj(&[("type", JsValue::from_str(envelope_type)), ("height", JsValue::from_f64(480.0))]);
        let event = message_from(FRAMED_MESSAGE_ORIGIN, &data);
        assert_eq!(read_embed_size(&event), None, "type {envelope_type:?} was not refused");
    }
}

#[wasm_bindgen_test]
fn a_height_that_is_not_a_finite_positive_number_is_refused() {
    // The caller sizes a frame from this value. A string that looks like a
    // number, a NaN, an infinity, and a zero or negative height are each a
    // report we cannot act on, and `None` leaves the frame at its default
    // rather than at whatever the arithmetic would have produced.
    let cases: [(&str, JsValue); 9] = [
        ("absent", JsValue::UNDEFINED),
        ("null", JsValue::NULL),
        ("a string", JsValue::from_str("480")),
        ("a boolean", JsValue::from_bool(true)),
        ("NaN", JsValue::from_f64(f64::NAN)),
        ("+Infinity", JsValue::from_f64(f64::INFINITY)),
        ("-Infinity", JsValue::from_f64(f64::NEG_INFINITY)),
        ("zero", JsValue::from_f64(0.0)),
        ("negative", JsValue::from_f64(-480.0)),
    ];
    for (shape, height) in cases {
        let event = message_from(FRAMED_MESSAGE_ORIGIN, &size_report(height));
        assert_eq!(read_embed_size(&event), None, "a height that was {shape} was accepted");
    }
}

// --- the two member readers both of the above are built on --------------------

#[wasm_bindgen_test]
fn message_field_treats_absent_and_empty_alike_and_refuses_non_strings() {
    let data = obj(&[
        ("present", JsValue::from_str("a value")),
        ("empty", JsValue::from_str("")),
        ("number", JsValue::from_f64(7.0)),
        ("boolean", JsValue::from_bool(true)),
        ("object", obj(&[("nested", JsValue::from_str("a value"))])),
        ("null", JsValue::NULL),
    ]);

    assert_eq!(message_field(&data, "present").as_deref(), Some("a value"));
    // idp.to omits a member it has nothing for, and a member that arrived
    // empty says nothing either — so both read as absent.
    assert_eq!(message_field(&data, "empty"), None);
    assert_eq!(message_field(&data, "absent"), None);
    // No coercion: "7" is not what arrived, and a caller comparing a state or
    // spending a code must not be handed a stringified anything.
    assert_eq!(message_field(&data, "number"), None);
    assert_eq!(message_field(&data, "boolean"), None);
    assert_eq!(message_field(&data, "object"), None);
    assert_eq!(message_field(&data, "null"), None);
}

#[wasm_bindgen_test]
fn message_field_on_a_value_that_cannot_hold_members_is_none_rather_than_a_panic() {
    // `event.data` is whatever the sender passed to `postMessage`, so it can be
    // a primitive; reading a member off null or undefined throws in JS, and
    // that has to come back as "no such member".
    for data in [JsValue::NULL, JsValue::UNDEFINED] {
        assert_eq!(message_field(&data, "type"), None);
    }
    assert_eq!(message_field(&JsValue::from_str("just a string"), "type"), None);
    assert_eq!(message_field(&JsValue::from_f64(1.0), "type"), None);
}

#[wasm_bindgen_test]
fn object_field_accepts_only_a_member_that_is_an_object() {
    let nested = obj(&[("code", JsValue::from_str("the-code"))]);
    let data = obj(&[
        ("object", nested),
        ("array", js_sys::Array::new().into()),
        ("null", JsValue::NULL),
        ("string", JsValue::from_str("not an object")),
        ("number", JsValue::from_f64(1.0)),
    ]);

    assert!(object_field(&data, "object").is_some());
    assert!(object_field(&data, "absent").is_none());
    assert!(object_field(&data, "null").is_none(), "null must not pass, however `typeof` describes it");
    assert!(object_field(&data, "string").is_none());
    assert!(object_field(&data, "number").is_none());
    // An array does pass this check — `typeof [] === "object"`. Recorded, not
    // endorsed: what stops an array envelope is the `state` comparison after
    // it, which is covered above.
    assert!(object_field(&data, "array").is_some());
}

// --- the request the ceremony sends out ---------------------------------------

#[wasm_bindgen_test]
fn the_authorization_query_carries_pkce_and_the_one_time_material_encoded() {
    // Values chosen to contain characters that must not survive unescaped into
    // a query string.
    let pending = PendingAuth {
        redirect_uri: "https://community.ankurah.org/auth/callback".to_string(),
        state: "st/ate+one".to_string(),
        nonce: "non ce".to_string(),
        challenge: "chal+lenge".to_string(),
    };
    let query = authorize_query(&pending);

    assert!(query.contains("response_type=code"), "{query}");
    assert!(query.contains("code_challenge_method=S256"), "{query}");
    assert!(query.contains(&format!("client_id={CLIENT_ID}")), "{query}");
    assert!(query.contains("redirect_uri=https%3A%2F%2Fcommunity.ankurah.org%2Fauth%2Fcallback"), "{query}");
    assert!(query.contains("state=st%2Fate%2Bone"), "{query}");
    assert!(query.contains("nonce=non%20ce"), "{query}");
    assert!(query.contains("code_challenge=chal%2Blenge"), "{query}");
    // `roles` is unconditional: our server requires the claim, so a role-less
    // request is useless rather than a fallback.
    assert!(query.contains("scope=openid%20profile%20email%20roles"), "{query}");
}

#[wasm_bindgen_test]
fn an_origin_idp_to_has_not_registered_as_an_embedder_gets_no_frame_and_stashes_nothing() {
    // Precondition rather than an assertion about the code: the test harness
    // serves this page from an ephemeral loopback port, and the registered
    // development embedder names port 5173 exactly.
    let origin = window().unwrap().location().origin().unwrap();
    assert!(!EMBED_ORIGINS.contains(&origin.as_str()), "the harness origin {origin} is a registered embedder; this test cannot run here");

    cancel_pending_sign_in();
    assert!(registered_embed_origin().is_none());
    // A frame the browser refuses to display reports nothing back, so an
    // unregistered origin is told to take the top-level flow instead — and the
    // top-level flow generates its own material, so nothing may be stashed here.
    assert!(matches!(begin_framed_sign_in(), Ok(None)), "an unregistered origin must get no framed attempt");
    let ss = session_storage().expect("sessionStorage");
    for key in [SS_VERIFIER, SS_STATE, SS_NONCE] {
        assert_eq!(ss.get_item(key).unwrap(), None, "{key} was stashed for an attempt that was never begun");
    }
}
