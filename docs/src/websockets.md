# WebSocket /ws

Connect to:

```text
ws://localhost:2236/ws?token=<token>
```

Use `wss://` when connecting through HTTPS. The token is checked during connection
setup. Expiration or revocation closes the socket with code 1008.

Every socket automatically receives its own `messages/<account_id>` topic. This does not
affect firehose ordering and cannot be explicitly joined or left.

## subscribe

Send any of these JSON events over the socket:

```json
{"event":"subscribe","id":"Sub1","data":{"type":"firehose"}}
```

```json
{"event":"subscribe","id":"Sub2","data":{"type":"user","uuid":"<account_id>"}}
```

```json
{"event":"subscribe","id":"Sub3","data":{"type":"collection","uuid":"<collection_uuid>"}}
```

| Topic | Receives |
| --- | --- |
| `firehose` | All listed collection and document creation events |
| `user/<uuid>` | That user's listed collection and document creation events |
| `collection/<uuid>` | Document creation events in that collection, including unlisted collections |

Success: `{"event":"subscribe_ok","id":"Sub1","data":{}}`.

Join the firehose before any other explicit topic. While subscribed, you can add
unlisted collections; user topics and listed collections are already covered. Duplicate
joins succeed without adding another membership.

Once you join any other topic, you cannot join the firehose on that connection, even
after unsubscribing. A connection that has joined only the firehose can leave and rejoin
it.

## unsubscribe

Use the same topic fields with `event: "unsubscribe"`:

```json
{"event":"unsubscribe","id":"Leave1","data":{"type":"collection","uuid":"<collection_uuid>"}}
```

Success: `{"event":"unsubscribe_ok","id":"Leave1","data":{}}`. Leaving a valid topic you
are not joined to is a successful no-op. Already queued events may still arrive
afterward; other overlapping subscriptions continue receiving events.

## Received events

Incoming WebSocket frames and complete messages, including fragmented messages, are
limited to 16 KiB of payload. Oversized text or binary input disconnects the socket
before application JSON parsing; a close frame is not guaranteed. The limit applies to
incoming commands, not outgoing publications.

Each socket has a bounded publication queue of 256 messages and a 1 MiB publication
payload budget, including an in-flight send. A socket that exceeds either limit is
removed from all topics and disconnected. Network sends time out after five seconds.
These disconnects may end the transport without a close frame; reconnect and use
catch-up for listed documents. Publications and direct messages remain transient;
accepting a publication into a queue does not guarantee delivery to the peer.

`create_collection_recv` contains `author` account ID, `uuid`, `name`, string `listed`,
and `signature`. It publishes to the author's user topic and firehose only when listed.

`create_document_recv` contains `author`, string `seq`, `type`, optional `name`, `uuid`,
`collection` UUID, `collection_name`, `refs`, and `signature`. It publishes to the
collection topic and, when listed, the author's user topic and firehose. It contains
metadata; fetch content through GET `/spa`.

`send_recv` arrives on every connected recipient socket:

```json
{"event":"send_recv","id":"Message1","data":{"from":"<sender_account_id>","content":{"text":"hello"}}}
```

Received events preserve the originating request's ID. Socket action failures use
`<action>_err` with `data.fakywaky`; malformed JSON, invalid IDs, or unknown events use
`generic_err`.

## Recovering after a disconnect

1. Subscribe and wait for `subscribe_ok`.
2. Buffer live document events while fetching catch-up pages from your saved checkpoint.
3. Merge the recovered documents and buffered events by UUID and sequence, applying each document once.
4. Save the catch-up snapshot as your new checkpoint after completing every page.

Live events can arrive out of order. Saving the largest live sequence as a checkpoint
can skip documents you have not received.

## Browser client example

Run this in a browser console to subscribe to the firehose and fetch new documents. Set
`base` to your server and replace `<token>` with a login token. The page must allow
connections to that server; HTTPS pages need an HTTPS URL.

```js
const base = new URL("http://localhost:2236");
const token = "<token>";
const socketUrl = new URL("/ws", base);
socketUrl.protocol = base.protocol === "https:" ? "wss:" : "ws:";
socketUrl.searchParams.set("token", token);

const socket = new WebSocket(socketUrl);
let readId = 0;

socket.addEventListener("open", () => {
  socket.send(JSON.stringify({
    event: "subscribe",
    id: "Sub1",
    data: { type: "firehose" }
  }));
});

socket.addEventListener("message", async (message) => {
  try {
    const event = JSON.parse(message.data);
    if (event.event === "subscribe_ok" && event.id === "Sub1") {
      console.log("Subscribed to new listed collections and documents");
    } else if (event.event === "create_document_recv") {
      const url = new URL("/spa", base);
      url.search = new URLSearchParams({
        event: "get_document",
        id: `Read${++readId}`,
        document: event.data.uuid
      }).toString();

      const response = await fetch(url);
      const result = await response.json();
      if (!response.ok) {
        throw new Error(result.data?.fakywaky ?? result.fakywaky ?? response.statusText);
      }
      console.log("New document:", result.data);
    } else if (event.event.endsWith("_err")) {
      console.error(event.event, event.data.fakywaky);
    } else {
      console.log(event.event, event.data);
    }
  } catch (error) {
    console.error("Could not process event:", error);
  }
});

socket.addEventListener("error", () => {
  console.error("WebSocket connection error");
});

socket.addEventListener("close", (event) => {
  console.log("Disconnected:", event.code, event.reason);
});

// Call socket.close() when you are done.
```

After `Subscribed` appears, create a document in a listed collection using HTTP. The
console will print the fetched document. Reads may finish in a different order from the
events that triggered them.

The example does not reconnect or recover missed documents. Add the recovery steps above
for that. An expired or revoked token requires a new login before reconnecting.
