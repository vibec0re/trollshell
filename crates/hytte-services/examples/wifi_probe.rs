//! End-to-end smoke test for the `NetworkManager` Wi-Fi backend.
//! Run with `cargo run -p hytte-services --example wifi_probe`.
//!
//! Drives [`hytte_services::wifi_nm::probe_snapshot`] against a live
//! `NetworkManager` on the system bus: confirms the backend, finds the Wi-Fi
//! device, reads state, triggers a scan, and reports the result. Backs the
//! `checks.wifi-nm-nixos-test` nixosTest, which boots NM + `mac80211_hwsim`
//! virtual radios in a VM.
//!
//! Prints greppable lines:
//! ```text
//!   wifi_probe: backend=NetworkManager
//!   wifi_probe: device=<path>
//!   wifi_probe: powered=<true|false>
//!   wifi_probe: station=<state>
//!   wifi_probe: scan=<ok|failed>
//!   wifi_probe: networks=<n>
//! ```

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    match rt.block_on(hytte_services::wifi_nm::probe_snapshot()) {
        Ok(snap) => {
            println!("wifi_probe: backend=NetworkManager");
            println!("wifi_probe: device={}", snap.device_path);
            println!("wifi_probe: powered={}", snap.powered);
            println!("wifi_probe: station={}", snap.station_state);
            println!(
                "wifi_probe: scan={}",
                if snap.scan_ok { "ok" } else { "failed" }
            );
            println!("wifi_probe: networks={}", snap.network_count);
        }
        Err(e) => {
            eprintln!("wifi_probe: ERROR: {e}");
            std::process::exit(1);
        }
    }
}
