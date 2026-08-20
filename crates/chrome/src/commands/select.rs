use crate::cdp::client::CdpClient;
use crate::element_ref::ElementRef;
use std::collections::HashMap;

pub async fn run(
    client: &CdpClient,
    uid_map: &HashMap<String, ElementRef>,
    uid: &str,
    value: &str,
) -> Result<String, crate::BoxError> {
    let text = crate::element::select_option(client, uid_map, uid, value).await?;
    Ok(format!("Selected \"{text}\" on uid={uid}"))
}
