// LSP-style Content-Length framing, wire-compatible with core's
// `core/src/protocol/jsonrpc.rs`. Header lines are `\n`-terminated (a
// trailing `\r` is tolerated), a blank line ends the header block, and the
// body is exactly the announced number of bytes - no trailing newline.

const CONTENT_LENGTH_HEADER = "content-length";

// Mirrors jsonrpc.rs's MAX_BODY_BYTES: a misbehaving peer must not be able to
// force an arbitrary allocation by announcing a huge body.
const MAX_BODY_BYTES = 64 * 1024 * 1024;

export function encodeFrame(body: Buffer): Buffer {
  const header = Buffer.from(`Content-Length: ${body.length}\r\n\r\n`, "ascii");
  return Buffer.concat([header, body]);
}

export function writeMessage(stream: NodeJS.WritableStream, message: unknown): void {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  stream.write(encodeFrame(body));
}

type ReaderState = "header" | "body";

/**
 * Incrementally reassembles Content-Length frames across arbitrarily chunked
 * input. `push` is the only entry point: feed it whatever bytes arrived on a
 * `data` event (or, in tests, however the bytes are sliced up) and it
 * returns the bodies of any frames that became complete as a result.
 */
export class FrameReader {
  private buffer: Buffer = Buffer.alloc(0);
  private state: ReaderState = "header";
  private expectedLength: number | null = null;
  /** Whether any header line has been seen in the block being parsed. A blank
   * line means two different things depending on this: end of the header
   * block, or padding between frames - see `consumeHeader`. */
  private sawHeaderLine = false;

  push(chunk: Buffer): Buffer[] {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    const frames: Buffer[] = [];

    for (;;) {
      if (this.state === "header") {
        if (!this.consumeHeader()) break;
      } else {
        if (this.expectedLength === null) {
          throw new Error("internal error: body state without a known length");
        }
        if (this.buffer.length < this.expectedLength) break;
        frames.push(this.buffer.subarray(0, this.expectedLength));
        this.buffer = this.buffer.subarray(this.expectedLength);
        this.expectedLength = null;
        this.sawHeaderLine = false;
        this.state = "header";
      }
    }

    return frames;
  }

  /** Consumes as many complete header lines as are buffered. Returns true
   * once the blank line ending the header block has been seen (state is
   * then switched to "body"); false if more data is needed first. */
  private consumeHeader(): boolean {
    for (;;) {
      const newline = this.buffer.indexOf(0x0a); // '\n'
      if (newline === -1) return false;

      let line = this.buffer.subarray(0, newline);
      if (line.length > 0 && line[line.length - 1] === 0x0d) {
        line = line.subarray(0, -1);
      }
      this.buffer = this.buffer.subarray(newline + 1);

      if (line.length === 0) {
        // A blank line before any header line is padding between frames, not
        // an empty header block. Tolerating it is what makes this reader
        // usable against `tsserver`, whose frames do not agree with their own
        // announced length on Windows:
        //
        //   `Content-Length: ${1 + len}\r\n\r\n${json}${newLine}`
        //
        // is what tsserver writes (TypeScript 5.9's `formatMessage`), with
        // `newLine` coming from `ts.sys.newLine`, which is `os.EOL`. On Unix
        // that is one byte and the arithmetic works out exactly. On Windows it
        // is two, so every frame leaves a stray `\n` behind, the next block
        // opens with what looks like a blank line, and a strict reader throws
        // `frame header is missing Content-Length` on the very first response
        // (GM-248). Every real tsserver client tolerates this; so must we.
        //
        // The strict check is kept for the case it was written for - a block
        // that had headers but no Content-Length among them, which is a
        // genuinely malformed frame rather than a byte of slack on the wire.
        if (!this.sawHeaderLine) continue;
        if (this.expectedLength === null) {
          throw new Error("frame header is missing Content-Length");
        }
        this.state = "body";
        return true;
      }

      this.sawHeaderLine = true;
      const text = line.toString("utf8");
      const colon = text.indexOf(":");
      if (colon === -1) {
        throw new Error(`malformed frame header line: ${JSON.stringify(text)}`);
      }
      const name = text.slice(0, colon).trim();
      const value = text.slice(colon + 1).trim();
      if (name.toLowerCase() === CONTENT_LENGTH_HEADER) {
        if (!/^\d+$/.test(value)) {
          throw new Error(`invalid Content-Length value: ${JSON.stringify(value)}`);
        }
        const length = Number(value);
        if (length > MAX_BODY_BYTES) {
          throw new Error(`frame body of ${length} bytes exceeds the ${MAX_BODY_BYTES}-byte limit`);
        }
        this.expectedLength = length;
      }
    }
  }
}
