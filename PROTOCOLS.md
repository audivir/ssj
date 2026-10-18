# Protocol versions

Every release speaks exactly one protocol version. A client and a server talk only if their
protocol versions are equal. The server file on a host is named `ssj-server-p<N>` after the
protocol.

| Release | Protocol |
| ------- | -------- |
| 0.1.0 | 1 |

Protocol 1 includes the `Gap` server frame (tag `0x87`). It was added before the first release, so
the golden file of protocol 1 was regenerated once and no released version speaks another format.
