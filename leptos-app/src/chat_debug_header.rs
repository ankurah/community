use leptos::prelude::*;

/// Debug header showing scroll manager state (mirrors the React ChatDebugHeader).
#[component]
pub fn ChatDebugHeader(
    #[prop(into)] mode: Signal<String>,
    #[prop(into)] has_more_preceding: Signal<bool>,
    #[prop(into)] has_more_following: Signal<bool>,
    #[prop(into)] should_auto_scroll: Signal<bool>,
    #[prop(into)] item_count: Signal<usize>,
) -> impl IntoView {
    let yesno = |b: bool| if b { "yes" } else { "no" };
    view! {
        <div class="debugHeader">
            <div class="debugRow">
                <span class="debugLabel">"Mode:"</span>
                <span class=move || format!("debugValue mode-{}", mode.get().to_lowercase())>{move || mode.get()}</span>
                <span class="debugLabel">"Items:"</span>
                <span class="debugValue">{move || item_count.get()}</span>
            </div>
            <div class="debugRow">
                <span class="debugLabel">"More preceding:"</span>
                <span class="debugValue">{move || yesno(has_more_preceding.get())}</span>
                <span class="debugLabel">"More following:"</span>
                <span class="debugValue">{move || yesno(has_more_following.get())}</span>
                <span class="debugLabel">"Auto-scroll:"</span>
                <span class="debugValue">{move || yesno(should_auto_scroll.get())}</span>
            </div>
        </div>
    }
}
