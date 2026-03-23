use std::io::Result;

fn main() -> Result<()> {
    // Use vendored protoc so cross-compilation (e.g. aarch64-unknown-linux-musl
    // via `cross`) works without needing protoc installed on the host or inside
    // the cross container.  protoc is a build-host tool so the x86_64 binary
    // bundled by protoc-bin-vendored is always the right one regardless of the
    // compilation target.
    // SAFETY: build scripts are single-threaded; no concurrent env reads.
    unsafe {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());
    }

    let proto_dir = "../../proto";

    let protos = &[
        format!("{proto_dir}/common.proto"),
        format!("{proto_dir}/aircd_http_api.proto"),
        format!("{proto_dir}/airc_ipc.proto"),
        format!("{proto_dir}/aircd_ipc.proto"),
        format!("{proto_dir}/relay.proto"),
    ];

    let mut config = prost_build::Config::new();

    // Add serde derives to all generated types so they can be serialized
    // as JSON (HTTP API) or binary protobuf (IPC).
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");

    // #[serde(default)] only works on structs, not enums. Apply selectively
    // to all message types (structs) but not enum types.
    // We target each package's messages explicitly.

    // common.proto messages
    config.type_attribute("airc.common.ChannelMessage", "#[serde(default)]");
    config.type_attribute("airc.common.ChannelStatus", "#[serde(default)]");
    config.type_attribute("airc.common.LogEvent", "#[serde(default)]");

    // http_api.proto messages
    config.type_attribute("airc.http_api.StatsResponse", "#[serde(default)]");
    config.type_attribute("airc.http_api.ChannelsResponse", "#[serde(default)]");
    config.type_attribute("airc.http_api.ChannelInfo", "#[serde(default)]");
    config.type_attribute("airc.http_api.ReputationResponse", "#[serde(default)]");
    config.type_attribute("airc.http_api.ErrorResponse", "#[serde(default)]");

    // airc_ipc.proto messages
    config.type_attribute("airc.ipc.IpcRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.JoinRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.PartRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.SayRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.FetchRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.StatusRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.DisconnectRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.LogStartRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.LogStopRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.LogsRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.IpcResponse", "#[serde(default)]");
    config.type_attribute("airc.ipc.TextPayload", "#[serde(default)]");
    config.type_attribute("airc.ipc.FetchPayload", "#[serde(default)]");
    config.type_attribute("airc.ipc.StatusPayload", "#[serde(default)]");
    config.type_attribute("airc.ipc.LogsPayload", "#[serde(default)]");
    config.type_attribute("airc.ipc.SilenceRequest", "#[serde(default)]");
    config.type_attribute("airc.ipc.FriendRequest", "#[serde(default)]");

    // aircd_ipc.proto messages
    config.type_attribute("airc.aircd_ipc.AircdRequest", "#[serde(default)]");
    config.type_attribute("airc.aircd_ipc.ShutdownRequest", "#[serde(default)]");
    config.type_attribute("airc.aircd_ipc.StatsRequest", "#[serde(default)]");
    config.type_attribute("airc.aircd_ipc.AircdResponse", "#[serde(default)]");
    config.type_attribute("airc.aircd_ipc.ShutdownResponse", "#[serde(default)]");
    config.type_attribute("airc.aircd_ipc.StatsResponse", "#[serde(default)]");
    config.type_attribute("airc.aircd_ipc.ChannelInfo", "#[serde(default)]");

    // relay.proto messages
    config.type_attribute("airc.relay.RelayEnvelope", "#[serde(default)]");
    config.type_attribute("airc.relay.ClientIntro", "#[serde(default)]");
    config.type_attribute("airc.relay.ClientDown", "#[serde(default)]");
    config.type_attribute("airc.relay.NickChange", "#[serde(default)]");
    config.type_attribute("airc.relay.Join", "#[serde(default)]");
    config.type_attribute("airc.relay.Part", "#[serde(default)]");
    config.type_attribute("airc.relay.Quit", "#[serde(default)]");
    config.type_attribute("airc.relay.Privmsg", "#[serde(default)]");
    config.type_attribute("airc.relay.Notice", "#[serde(default)]");
    config.type_attribute("airc.relay.Topic", "#[serde(default)]");
    config.type_attribute("airc.relay.Mode", "#[serde(default)]");
    config.type_attribute("airc.relay.Kick", "#[serde(default)]");
    config.type_attribute("airc.relay.NodeUp", "#[serde(default)]");
    config.type_attribute("airc.relay.NodeDown", "#[serde(default)]");
    config.type_attribute("airc.relay.CrdtDelta", "#[serde(default)]");
    config.type_attribute("airc.relay.AntiEntropyRequest", "#[serde(default)]");
    config.type_attribute("airc.relay.AntiEntropyResponse", "#[serde(default)]");
    config.type_attribute("airc.relay.StateSnapshot", "#[serde(default)]");
    config.type_attribute("airc.relay.SnapshotClient", "#[serde(default)]");
    config.type_attribute("airc.relay.SnapshotChannel", "#[serde(default)]");
    config.type_attribute("airc.relay.SnapshotMembership", "#[serde(default)]");

    config.compile_protos(protos, &[proto_dir])?;

    Ok(())
}
