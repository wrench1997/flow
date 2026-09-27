# Flow local patch

Based on librqbit-peer-protocol 9.0.1, rqbit commit a499d2f243d124e144aef137afe7cb304a6e3f36, crates/peer_binary_protocol (Apache-2.0).

Expose the validated metadata data message total_size through a read-only accessor. Flow uses it to initialize a bounded shared collector when a peer omits metadata_size in its extension handshake. Existing wire validation is unchanged.
