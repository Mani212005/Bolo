use super::TextInjector;
use anyhow::{anyhow, Context};
use ashpd::desktop::remote_desktop::{
    DeviceType, KeyState, NotifyKeyboardKeysymOptions, RemoteDesktop, SelectDevicesOptions,
    StartOptions,
};
use ashpd::desktop::Session;

// X11 keysyms for the control characters we can meaningfully type.
const XK_RETURN: u32 = 0xFF0D;
const XK_TAB: u32 = 0xFF09;
const XK_CONTROL_L: u32 = 0xFFE3;

/// Types text into the focused app by synthesizing keystrokes through the
/// XDG RemoteDesktop portal.
///
/// The session is created lazily on the first injection and then held for
/// the daemon's lifetime: this portal is v1 on GNOME 42 (no restore tokens),
/// so a fresh session would mean a permission dialog per utterance.
/// TODO(portal v2 / xdg-desktop-portal >= 1.16): persist with a restore
/// token so the dialog disappears entirely across daemon restarts.
pub struct PortalInjector {
    delay: std::time::Duration,
    session: Option<(RemoteDesktop, Session<RemoteDesktop>)>,
}

impl PortalInjector {
    pub fn new(type_delay_ms: u64) -> Self {
        Self { delay: std::time::Duration::from_millis(type_delay_ms), session: None }
    }

    async fn ensure_session(
        &mut self,
    ) -> anyhow::Result<()> {
        if self.session.is_none() {
            let proxy = RemoteDesktop::new().await.context("RemoteDesktop portal unavailable")?;
            let session = proxy
                .create_session(Default::default())
                .await
                .context("portal CreateSession failed")?;
            proxy
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(ashpd::enumflags2::BitFlags::from(DeviceType::Keyboard)),
                )
                .await
                .context("portal SelectDevices failed")?
                .response()?;
            // This pops the one-time permission dialog on GNOME 42.
            let devices = proxy
                .start(&session, None, StartOptions::default())
                .await
                .context("portal Start failed")?
                .response()
                .map_err(|e| anyhow!("portal permission denied or dialog dismissed: {e}"))?;
            if !devices.devices().contains(DeviceType::Keyboard) {
                return Err(anyhow!("portal session granted without keyboard access"));
            }
            self.session = Some((proxy, session));
        }
        Ok(())
    }

    async fn key(&self, keysym: u32, state: KeyState) -> anyhow::Result<()> {
        let (proxy, session) = self.session.as_ref().expect("session ensured before key");
        proxy
            .notify_keyboard_keysym(session, keysym as i32, state, NotifyKeyboardKeysymOptions::default())
            .await?;
        Ok(())
    }

    async fn tap(&self, keysym: u32) -> anyhow::Result<()> {
        self.key(keysym, KeyState::Pressed).await?;
        self.key(keysym, KeyState::Released).await
    }

    /// Synthesize Ctrl+V at the focused app — the "paste it all at once"
    /// path: the transcript is placed on the clipboard first, then this
    /// chord makes the app paste it in one go.
    pub async fn paste_chord(&mut self) -> anyhow::Result<()> {
        self.ensure_session().await?;
        let gap = std::time::Duration::from_millis(10);
        self.key(XK_CONTROL_L, KeyState::Pressed).await?;
        tokio::time::sleep(gap).await;
        self.key('v' as u32, KeyState::Pressed).await?;
        tokio::time::sleep(gap).await;
        self.key('v' as u32, KeyState::Released).await?;
        tokio::time::sleep(gap).await;
        self.key(XK_CONTROL_L, KeyState::Released).await?;
        Ok(())
    }
}

/// Map a char to an X11 keysym: ASCII printable maps 1:1, everything else
/// uses the Unicode range (0x01000000 + codepoint).
fn keysym_for(c: char) -> Option<u32> {
    match c {
        '\n' | '\r' => Some(XK_RETURN),
        '\t' => Some(XK_TAB),
        ' '..='~' => Some(c as u32),
        c if (c as u32) >= 0xA0 => Some(0x0100_0000 + c as u32),
        _ => None, // other control chars: skip
    }
}

#[async_trait::async_trait]
impl TextInjector for PortalInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        self.ensure_session().await?;
        for c in text.chars() {
            if let Some(keysym) = keysym_for(c) {
                self.tap(keysym).await?;
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
            }
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "portal"
    }
}
