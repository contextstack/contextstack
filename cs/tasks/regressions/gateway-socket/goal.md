# gateway-socket

Cs-sandbox's `net: gateway`
(phase 3a/3b) had only been tested against a stand-in socket; the gateway listened on TCP only.

The check must show, against a real `csgateway` built from `modelgateway/` and started with
no `*_API_KEY` in its environment, `CSGATEWAY_SOCKET` set and a bearer token:

1. The socket exists with mode 0600 beside the TCP listener.
2. Over the socket and over TCP, `GET /v1/models` (valid, missing and wrong token) and
   `GET /v1/capabilities` return the same status and body (`providers_active` compared
   sorted: the gateway emits it in map order on either transport). Auth is not bypassed:
   no token over the socket is 401.
3. Through the release `cs-sandbox` with a policy allowing `gateway`, a tool whose
   descriptor requests `net: gateway`, with `CS_GATEWAY_SOCKET` naming the socket, gets
   the `/v1/models` body over the socket, and in the same process a TCP connect to the
   gateway's live port (reachable from outside the sandbox) and to 127.0.0.1:9200 fails.
   Control: the same tool under `net: none` cannot reach the socket.
4. SIGTERM shuts the gateway down and removes the socket.

Skipped, with the reason printed, when there is no Go toolchain and no `CSGATEWAY_BIN`;
part 3 is skipped when the wrapper is not built.
