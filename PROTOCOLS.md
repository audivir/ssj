# Protocol versions

Every release speaks exactly one protocol version. A client and a server talk only if their
protocol versions are equal. The server file on a host is named `ssj-server-p<N>` after the
protocol.

| Release | Protocol |
| ------- | -------- |
| 0.1.0 | 1 |

Protocol 1 includes the `Gap` and `Compressed` frames and the frames of agent forwarding. They were
added before the first release, so the golden file of protocol 1 was regenerated and no released
version speaks another format.
