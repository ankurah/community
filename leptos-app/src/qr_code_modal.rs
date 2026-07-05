use leptos::prelude::*;
use qrcode::QrCode;
use qrcode::render::svg;

/// Modal displaying a QR code for connecting to the chat from mobile devices.
#[component]
pub fn QRCodeModal(url: String, on_close: impl Fn() + Clone + 'static) -> impl IntoView {
    // Generate QR code SVG
    let qr_svg = match QrCode::new(&url) {
        Ok(code) => {
            let svg_string = code.render::<svg::Color>().min_dimensions(256, 256).build();
            svg_string
        }
        Err(_) => String::from("<svg></svg>"),
    };

    let on_close_overlay = on_close.clone();
    let on_close_button = on_close.clone();

    view! {
        <div class="qrModalOverlay" on:click=move |_| on_close_overlay()>
            <div class="qrModalContent" on:click=|e| e.stop_propagation()>
                <div class="qrModalHeader">
                    <h2>"Scan to Connect to Ankurah Community"</h2>
                    <button class="qrCloseButton" on:click=move |_| on_close_button()>
                        "×"
                    </button>
                </div>
                <div class="qrCodeContainer" inner_html=qr_svg></div>
                <div class="qrUrlDisplay">
                    <code>{url.clone()}</code>
                </div>
                <p class="qrInstructions">
                    "Scan this QR code with your mobile device to open the app"
                </p>
            </div>
        </div>
    }
}
