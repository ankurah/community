use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ankurah::{LiveQuery, changes::ChangeSet};
use ankurah_signals::{Get, Mut, Peek, Subscribe, SubscriptionGuard};
use community_model::{MessageView, RoomView};
use send_wrapper::SendWrapper;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{AudioBuffer, AudioContext};

use crate::ctx;

/// Manages notification sounds and unread message counts per room.
///
/// Uses one query per room (since GROUP BY is not yet available in Ankurah).
/// Tracks messages from other users and plays notification sounds.
#[derive(Clone)]
pub struct NotificationManager(SendWrapper<Arc<Inner>>);

struct Inner {
    current_user_id: Mutex<Option<String>>,
    active_room_id: Mutex<Option<String>>,
    room_queries: Mutex<HashMap<String, RoomQueryState>>,
    audio_context: SendWrapper<AudioContext>,
    audio_buffer: Mutex<Option<SendWrapper<AudioBuffer>>>,
    last_sound_played_at: Mutex<f64>,
    unread_counts: Mut<HashMap<String, usize>>,
    _rooms_guard: Mutex<Option<SubscriptionGuard>>,
}

struct RoomQueryState {
    _query: LiveQuery<MessageView>,
    _guard: SubscriptionGuard,
}

impl NotificationManager {
    pub fn new(rooms: LiveQuery<RoomView>, current_user: Option<String>) -> Self {
        let audio_context = AudioContext::new().expect("Failed to create AudioContext");
        let unread_counts = Mut::new(HashMap::new());

        let inner = Arc::new(Inner {
            current_user_id: Mutex::new(current_user),
            active_room_id: Mutex::new(None),
            room_queries: Mutex::new(HashMap::new()),
            audio_context: SendWrapper::new(audio_context.clone()),
            audio_buffer: Mutex::new(None),
            last_sound_played_at: Mutex::new(0.0),
            unread_counts: unread_counts.clone(),
            _rooms_guard: Mutex::new(None),
        });

        // Load notification sound
        Self::load_audio_file(inner.clone());

        // Set up audio unlock for iOS
        Self::setup_audio_unlock(audio_context);

        // Subscribe to room changes
        let inner_for_sub = inner.clone();
        let rooms_guard = rooms.subscribe(move |changeset: ChangeSet<RoomView>| {
            // Add queries for new rooms
            for room in changeset.appeared() {
                Self::add_room_query(inner_for_sub.clone(), room);
            }

            // Remove queries for removed rooms
            for room in changeset.removed() {
                Self::remove_room_query(inner_for_sub.clone(), room.id().to_base64());
            }
        });

        // Store the guard
        *inner._rooms_guard.lock().unwrap() = Some(rooms_guard);

        Self(SendWrapper::new(inner))
    }

    fn setup_audio_unlock(audio_context: AudioContext) {
        let unlock = Closure::wrap(Box::new(move || {
            if audio_context.state() == web_sys::AudioContextState::Suspended {
                let _ = audio_context.resume();
                tracing::info!("AudioContext resumed on user interaction");
            }
        }) as Box<dyn FnMut()>);

        if let Some(window) = web_sys::window() {
            if let Some(document) = window.document() {
                let _ = document.add_event_listener_with_callback("touchstart", unlock.as_ref().unchecked_ref());
                let _ = document.add_event_listener_with_callback("click", unlock.as_ref().unchecked_ref());
            }
        }

        unlock.forget(); // Keep the closure alive
    }

    fn load_audio_file(inner: Arc<Inner>) {
        spawn_local(async move {
            match Self::fetch_and_decode_audio(&inner.audio_context).await {
                Ok(buffer) => {
                    *inner.audio_buffer.lock().unwrap() = Some(SendWrapper::new(buffer));
                    tracing::info!("Notification sound loaded successfully");
                }
                Err(e) => {
                    tracing::error!("Failed to load notification sound: {:?}", e);
                }
            }
        });
    }

    async fn fetch_and_decode_audio(audio_context: &AudioContext) -> Result<AudioBuffer, JsValue> {
        use wasm_bindgen::JsCast;

        let window = web_sys::window().ok_or("No window")?;
        let resp = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str("/sounds/notification.mp3")).await?;
        let resp: web_sys::Response = resp.dyn_into()?;
        let array_buffer_promise = resp.array_buffer()?;
        let array_buffer = wasm_bindgen_futures::JsFuture::from(array_buffer_promise).await?;
        let array_buffer: js_sys::ArrayBuffer = array_buffer.dyn_into()?;
        let decode_promise = audio_context.decode_audio_data(&array_buffer)?;
        let buffer = wasm_bindgen_futures::JsFuture::from(decode_promise).await?;
        Ok(buffer.dyn_into()?)
    }

    fn add_room_query(inner: Arc<Inner>, room: RoomView) {
        let room_id = room.id().to_base64();

        if inner.room_queries.lock().unwrap().contains_key(&room_id) {
            return;
        }

        // Create lightweight query for latest messages in this room
        let predicate = format!("room = '{}' AND deleted = false ORDER BY timestamp DESC LIMIT 10", room_id);
        let query = match ctx().query::<MessageView>(predicate.as_str()) {
            Ok(q) => q,
            Err(e) => {
                tracing::error!("Failed to create message query for room {}: {:?}", room_id, e);
                return;
            }
        };

        let inner_for_sub = inner.clone();
        let room_id_for_sub = room_id.clone();
        let notification_count = Arc::new(Mutex::new(0usize));

        let guard = query.subscribe(move |changeset: ChangeSet<MessageView>| {
            let mut count = notification_count.lock().unwrap();
            *count += 1;

            // Skip initial load
            if *count == 1 {
                return;
            }

            // After initial load, any adds from other users trigger notification
            let current_user_id = inner_for_sub.current_user_id.lock().unwrap();
            let new_messages_from_others: Vec<_> = changeset
                .appeared()
                .into_iter()
                .filter(|msg| {
                    // msg.user() returns Result<Ref<User>>; compare by base64 id
                    let msg_user_id = msg.user().ok().map(|r| r.id().to_base64());
                    msg_user_id.as_deref() != current_user_id.as_deref()
                })
                .collect();

            if !new_messages_from_others.is_empty() {
                tracing::info!("NotificationManager: {} new messages from others", new_messages_from_others.len());

                // Only increment unread count if not the active room
                let active_room_id = inner_for_sub.active_room_id.lock().unwrap();
                let is_active_room = active_room_id.as_ref() == Some(&room_id_for_sub);

                if !is_active_room {
                    let mut counts = inner_for_sub.unread_counts.peek().clone();
                    let new_count = counts.get(&room_id_for_sub).unwrap_or(&0) + new_messages_from_others.len();
                    counts.insert(room_id_for_sub.clone(), new_count);
                    inner_for_sub.unread_counts.set(counts);
                }

                // Always play sound for messages from others (even in active room)
                Self::play_notification_sound(inner_for_sub.clone());
            }
        });

        inner
            .room_queries
            .lock()
            .unwrap()
            .insert(room_id, RoomQueryState { _query: query, _guard: guard });
    }

    fn remove_room_query(inner: Arc<Inner>, room_id: String) {
        inner.room_queries.lock().unwrap().remove(&room_id);

        // Remove unread count for this room
        let mut counts = inner.unread_counts.peek().clone();
        counts.remove(&room_id);
        inner.unread_counts.set(counts);
    }

    fn play_notification_sound(inner: Arc<Inner>) {
        const SOUND_DEBOUNCE_MS: f64 = 300.0;
        const VOLUME: f32 = 0.1;

        let now = js_sys::Date::now();
        let last_played = *inner.last_sound_played_at.lock().unwrap();

        // Debounce: don't play if we just played recently
        if now - last_played < SOUND_DEBOUNCE_MS {
            return;
        }

        let audio_buffer = inner.audio_buffer.lock().unwrap();
        let Some(buffer_wrapper) = audio_buffer.as_ref() else {
            tracing::debug!("Audio buffer not loaded yet");
            return;
        };
        let buffer = &**buffer_wrapper;

        *inner.last_sound_played_at.lock().unwrap() = now;

        // Resume audio context if suspended (iOS requirement)
        if inner.audio_context.state() == web_sys::AudioContextState::Suspended {
            let _ = inner.audio_context.resume();
        }

        // Create a new buffer source for this playback
        let source = match inner.audio_context.create_buffer_source() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to create buffer source: {:?}", e);
                return;
            }
        };

        source.set_buffer(Some(buffer));

        // Create gain node for volume control
        let gain_node = match inner.audio_context.create_gain() {
            Ok(g) => g,
            Err(e) => {
                tracing::error!("Failed to create gain node: {:?}", e);
                return;
            }
        };

        gain_node.gain().set_value(VOLUME);

        // Connect: source -> gain -> destination
        if let Err(e) = source.connect_with_audio_node(&gain_node) {
            tracing::error!("Failed to connect source to gain: {:?}", e);
            return;
        }

        if let Err(e) = gain_node.connect_with_audio_node(&inner.audio_context.destination()) {
            tracing::error!("Failed to connect gain to destination: {:?}", e);
            return;
        }

        // Play the sound
        if let Err(e) = source.start() {
            tracing::error!("Failed to start audio source: {:?}", e);
        }
    }

    /// Reactive unread count for one room (tracks the signal, so a badge that
    /// reads this re-renders when the count changes).
    pub fn unread_count(&self, room_id: &str) -> usize {
        self.0.unread_counts.get().get(room_id).copied().unwrap_or(0)
    }

    /// Update the current user's id. Notifications only fire for messages from
    /// *other* users, so this must be set once the async user load completes —
    /// otherwise (id = None) your own messages are treated as someone else's.
    pub fn set_current_user_id(&self, id: Option<String>) {
        *self.0.current_user_id.lock().unwrap() = id;
    }

    /// Set the currently active room (for marking messages as read).
    /// Pass None to clear the active room.
    pub fn set_active_room(&self, room_id: Option<String>) {
        *self.0.active_room_id.lock().unwrap() = room_id.clone();
        if let Some(room_id) = room_id {
            self.mark_as_read(&room_id);
        }
    }

    fn mark_as_read(&self, room_id: &str) {
        let mut counts = self.0.unread_counts.peek().clone();
        counts.remove(room_id);
        self.0.unread_counts.set(counts);
    }
}
