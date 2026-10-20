# Protocol versions

Every release speaks exactly one protocol version. A client and a server talk only if their
protocol versions are equal. The server file on a host is named `ssj-server-p<N>` after the
protocol.

| Release | Protocol |
| ------- | -------- |
| 0.1.0-rc4 | 1 |
| 0.1.0-rc5 | 1 |
| 0.1.0-rc6 | 1 |
| 0.1.0 | 1 |

Protocol 1 includes the `Gap`, `Compressed`, and `Detached` frames and the frames of agent
forwarding. The earlier pre-releases were withdrawn, so no published release speaks another
format of protocol 1.
