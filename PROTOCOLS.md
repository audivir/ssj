# Protocol versions

Every release speaks exactly one protocol version. A client and a server talk only if their
protocol versions are equal. The server file on a host is named `ssj-server-p<N>` after the
protocol.

| Release | Protocol |
| ------- | -------- |
| 0.1.0-rc1 | 1 |
| 0.1.0-rc2 | 1 |
| 0.1.0-rc3 | 1 |
| 0.1.0 | 1 |

Protocol 1 includes the `Gap` and `Compressed` frames and the frames of agent forwarding. They were
added before the first release, so the golden file of protocol 1 was regenerated and no released
version speaks another format.

The `Detached` frame (`ssj detach`) came after 0.1.0-rc2 and the golden file was regenerated for
it. A client of 0.1.0-rc1 or 0.1.0-rc2 that is detached by a newer server stops with an error about
an unknown frame; the session keeps running.
