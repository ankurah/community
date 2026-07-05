use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::KeyboardEvent;

/// Editable text field. Shows a value; on click, switches to an input that calls
/// `on_change` with the new text on every edit (the caller persists it). `value`
/// is reactive, so remote/committed changes update the display.
#[component]
pub fn EditableTextField(
    /// The current value to display (reactive).
    #[prop(into)]
    value: Signal<String>,
    /// Callback when the value changes.
    on_change: impl Fn(String) + Clone + Send + Sync + 'static,
    #[prop(optional)] placeholder: Option<String>,
    #[prop(optional)] class: Option<String>,
) -> impl IntoView {
    let is_editing = RwSignal::new(false);
    let local_value = RwSignal::new(String::new());
    let cursor_pos = RwSignal::new(0);
    let input_ref = NodeRef::<leptos::html::Input>::new();

    let placeholder = placeholder.unwrap_or_else(|| "Click to edit".to_string());
    let class_name = class.unwrap_or_default();

    // Focus and set cursor position when entering edit mode.
    Effect::new(move |_| {
        if is_editing.get() {
            if let Some(input_el) = input_ref.get() {
                let _ = input_el.focus();
                let pos = cursor_pos.get() as u32;
                let _ = input_el.set_selection_range(pos, pos);
            }
        }
    });

    let start_edit = move |_| {
        let current = value.get_untracked();
        cursor_pos.set(current.len());
        local_value.set(current);
        is_editing.set(true);
    };

    let handle_change = {
        let on_change = on_change.clone();
        move |ev: web_sys::Event| {
            if let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) {
                let new_value = input.value();
                let new_cursor_pos = input.selection_start().ok().flatten().unwrap_or(0) as usize;
                on_change(new_value.clone());
                local_value.set(new_value);
                cursor_pos.set(new_cursor_pos);
            }
        }
    };

    let end_edit = move || {
        is_editing.set(false);
        local_value.set(String::new());
    };

    let handle_key_down = move |ev: KeyboardEvent| {
        if ev.key() == "Enter" || ev.key() == "Escape" {
            ev.prevent_default();
            end_edit();
        }
    };

    view! {
        <Show
            when=move || is_editing.get()
            fallback={
                let placeholder = placeholder.clone();
                let class_name = class_name.clone();
                move || {
                    let title = placeholder.clone();
                    let empty_placeholder = placeholder.clone();
                    view! {
                        <span
                            class=format!("editableText {}", class_name)
                            on:click=start_edit
                            title=title
                        >
                            {move || {
                                let v = value.get();
                                if v.is_empty() { empty_placeholder.clone() } else { v }
                            }}
                        </span>
                    }
                }
            }
        >
            {
                let handle_change = handle_change.clone();
                let handle_key_down = handle_key_down.clone();
                let class_name = class_name.clone();
                move || view! {
                    <input
                        node_ref=input_ref
                        type="text"
                        class=format!("editableInput {}", class_name)
                        prop:value=move || local_value.get()
                        on:input=handle_change.clone()
                        on:keydown=handle_key_down.clone()
                        on:blur=move |_| end_edit()
                    />
                }
            }
        </Show>
    }
}
