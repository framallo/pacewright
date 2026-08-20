use crate::cdp::client::CdpClient;
use crate::element_ref::ElementRef;
use std::collections::HashMap;

pub async fn run(
    client: &CdpClient,
    uid_map: &HashMap<String, ElementRef>,
    uid: &str,
) -> Result<String, crate::BoxError> {
    crate::element::dblclick(client, uid_map, uid).await?;
    Ok(format!("Double-clicked uid={uid}"))
}
