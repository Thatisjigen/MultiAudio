//! macOS: the volume indicator shown when a volume key is pressed (macOS has
//! none for the "MultiAudio" output). A small dark panel in the top-right
//! corner, above everything (full-screen videos too), never taking focus.

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAnimationContext, NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSBox, NSBoxType, NSColor, NSFont, NSFontWeightMedium, NSFontWeightRegular, NSImage, NSImageSymbolConfiguration, NSImageView, NSPanel,
    NSScreen, NSStatusWindowLevel, NSTextAlignment, NSTextField, NSTitlePosition, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

const W: f64 = 290.0;
const H: f64 = 58.0;
const BAR_X: f64 = 52.0;
const BAR_W: f64 = W - BAR_X - 18.0;

static ENABLED: AtomicBool = AtomicBool::new(false);
static GEN: AtomicU64 = AtomicU64::new(0);

thread_local! {
    // main thread only
    static HUD: RefCell<Option<Hud>> = const { RefCell::new(None) };
}

struct Hud {
    panel: Retained<NSPanel>,
    icon: Retained<NSImageView>,
    fill: Retained<NSBox>,
    pct: Retained<NSTextField>,
}

/// Only where AppKit runs (the window app, not `--headless`).
pub fn enable() {
    ENABLED.store(true, Relaxed);
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn bar(mtm: MainThreadMarker, r: NSRect, color: &NSColor) -> Retained<NSBox> {
    let b = NSBox::initWithFrame(NSBox::alloc(mtm), r);
    b.setBoxType(NSBoxType::Custom);
    b.setTitlePosition(NSTitlePosition::NoTitle);
    b.setBorderWidth(0.0);
    b.setCornerRadius(r.size.height / 2.0);
    b.setFillColor(color);
    b.setContentViewMargins(NSSize::new(0.0, 0.0));
    b
}

fn label(mtm: MainThreadMarker, text: &str, r: NSRect, font: &NSFont, color: &NSColor) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFrame(r);
    l.setFont(Some(font));
    l.setTextColor(Some(color));
    l
}

impl Hud {
    fn new(mtm: MainThreadMarker) -> Hud {
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, W, H),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setIgnoresMouseEvents(true);
        panel.setHidesOnDeactivate(false);
        panel.setLevel(NSStatusWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            panel.setAppearance(Some(&dark));
        }

        let bg = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect(0.0, 0.0, W, H));
        bg.setMaterial(NSVisualEffectMaterial::HUDWindow);
        bg.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        bg.setState(NSVisualEffectState::Active);
        bg.setWantsLayer(true);
        unsafe {
            let layer: Option<Retained<AnyObject>> = msg_send![&*bg, layer];
            if let Some(layer) = layer {
                let _: () = msg_send![&*layer, setCornerRadius: 16.0f64];
                let _: () = msg_send![&*layer, setMasksToBounds: true];
            }
        }
        let root: &NSView = &bg;

        let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(14.0, 15.0, 28.0, 28.0));
        icon.setContentTintColor(Some(&NSColor::whiteColor()));
        icon.setSymbolConfiguration(Some(&NSImageSymbolConfiguration::configurationWithPointSize_weight(17.0, unsafe { NSFontWeightMedium })));
        root.addSubview(&icon);

        let dim = NSColor::colorWithWhite_alpha(1.0, 0.75);
        let title = label(mtm, "MultiAudio", rect(BAR_X - 2.0, 29.0, 150.0, 17.0), &NSFont::boldSystemFontOfSize(12.0), &dim);
        root.addSubview(&title);
        let pct = label(
            mtm,
            "",
            rect(W - 18.0 - 70.0, 29.0, 70.0, 17.0),
            &NSFont::monospacedDigitSystemFontOfSize_weight(12.0, unsafe { NSFontWeightRegular }),
            &dim,
        );
        pct.setAlignment(NSTextAlignment::Right);
        root.addSubview(&pct);

        let track = bar(mtm, rect(BAR_X, 16.0, BAR_W, 6.0), &NSColor::colorWithWhite_alpha(1.0, 0.2));
        root.addSubview(&track);
        let fill = bar(mtm, rect(BAR_X, 16.0, BAR_W, 6.0), &NSColor::whiteColor());
        root.addSubview(&fill);

        panel.setContentView(Some(root));
        Hud { panel, icon, fill, pct }
    }

    fn update(&self, mtm: MainThreadMarker, level: f32, muted: bool) {
        let symbol = if muted || level <= 0.0 {
            "speaker.slash.fill"
        } else if level < 0.34 {
            "speaker.wave.1.fill"
        } else if level < 0.67 {
            "speaker.wave.2.fill"
        } else {
            "speaker.wave.3.fill"
        };
        let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), None);
        self.icon.setImage(img.as_deref());
        let w = BAR_W * level.clamp(0.0, 1.0) as f64;
        self.fill.setHidden(w < 1.0);
        self.fill.setFrame(rect(BAR_X, 16.0, w.max(6.0), 6.0));
        let color = if muted { NSColor::colorWithWhite_alpha(1.0, 0.35) } else { NSColor::whiteColor() };
        self.fill.setFillColor(&color);
        let text = if muted { "muto".to_string() } else { format!("{:.0} %", level * 100.0) };
        self.pct.setStringValue(&NSString::from_str(&text));
        // top-right corner of the screen in use, under the menu bar
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let f = screen.visibleFrame();
            self.panel.setFrameOrigin(NSPoint::new(f.origin.x + f.size.width - W - 14.0, f.origin.y + f.size.height - H - 10.0));
        }
        // cancel a fade-out in progress
        NSAnimationContext::beginGrouping();
        NSAnimationContext::currentContext().setDuration(0.0);
        self.panel.animator().setAlphaValue(1.0);
        NSAnimationContext::endGrouping();
        self.panel.orderFrontRegardless();
        self.panel.invalidateShadow();
    }

    fn fade_out(&self) {
        NSAnimationContext::beginGrouping();
        NSAnimationContext::currentContext().setDuration(0.25);
        self.panel.animator().setAlphaValue(0.0);
        NSAnimationContext::endGrouping();
    }
}

fn after(ms: u64, f: impl FnOnce() + Send + 'static) {
    let when = DispatchTime::try_from(Duration::from_millis(ms)).unwrap_or(DispatchTime::NOW);
    let _ = DispatchQueue::main().after(when, f);
}

/// Shows the indicator for a moment (callable from any thread).
pub fn show(level: f32, muted: bool) {
    if !ENABLED.load(Relaxed) {
        return;
    }
    let gen = GEN.fetch_add(1, Relaxed) + 1;
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        HUD.with(|h| h.borrow_mut().get_or_insert_with(|| Hud::new(mtm)).update(mtm, level, muted));
    });
    after(1500, move || {
        if GEN.load(Relaxed) == gen {
            HUD.with(|h| {
                if let Some(hud) = h.borrow().as_ref() {
                    hud.fade_out();
                }
            });
            after(300, move || {
                if GEN.load(Relaxed) == gen {
                    HUD.with(|h| {
                        if let Some(hud) = h.borrow().as_ref() {
                            hud.panel.orderOut(None);
                        }
                    });
                }
            });
        }
    });
}
