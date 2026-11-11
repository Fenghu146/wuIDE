use client::{LEGACY_URL_SCHEME, ZED_URL_SCHEME};
use gpui::{AsyncApp, actions};

actions!(
    cli,
    [
        /// Registers the ide:// URL scheme handler.
        RegisterWuScheme
    ]
);

pub async fn register_wu_scheme(cx: &AsyncApp) -> anyhow::Result<()> {
    cx.update(|cx| cx.register_url_scheme(ZED_URL_SCHEME))
        .await?;
    cx.update(|cx| cx.register_url_scheme(LEGACY_URL_SCHEME))
        .await?;
    Ok(())
}
