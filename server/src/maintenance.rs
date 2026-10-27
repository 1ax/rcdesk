//! Суточная уборка БД (долг D53): пустые анонимные владельцы и устройства без
//! владельца. Общая для `rcdesk-server` и LAN-хаба хоста (D47).

use crate::db::{Db, DEVICE_IDLE_TTL_SECS, OWNER_IDLE_TTL_SECS, SESSION_RETENTION_SECS};
use crate::ice::now_unix;
use crate::registry::Registry;

/// Раз в сутки (и сразу при старте) чистит пустых анонимных владельцев и
/// устройства без владельца. Короткий запрос под std-мьютексом, как остальной
/// код.
pub fn spawn_maintenance(db: Db, registry: Registry) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
        loop {
            interval.tick().await;
            match db.prune_empty_owners(now_unix(), OWNER_IDLE_TTL_SECS) {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} empty owners"),
                Err(err) => tracing::warn!("failed to prune empty owners: {err:#}"),
            }
            match db.prune_unowned_devices(
                now_unix(),
                DEVICE_IDLE_TTL_SECS,
                &registry.online_host_ids(),
            ) {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} unowned devices"),
                Err(err) => tracing::warn!("failed to prune unowned devices: {err:#}"),
            }
            match db.prune_sessions(now_unix(), SESSION_RETENTION_SECS) {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} old session log rows"),
                Err(err) => tracing::warn!("failed to prune session log: {err:#}"),
            }
        }
    })
}
