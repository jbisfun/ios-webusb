// Browser client for ios-webusb: the usbmuxd protocol over the local WebSocket bridge.
// Each WebSocket is one usbmuxd connection, exactly like a socket to /var/run/usbmuxd.

import plist from "plist";

export const DEFAULT_URL = "ws://127.0.0.1:27150";

const HEADER_SIZE = 16;
const VERSION_PLIST = 1;
const MESSAGE_PLIST = 8;
const LOCKDOWN_PORT = 62078;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** A byte stream over one bridge WebSocket. */
export class Stream {
  #socket;
  #chunks = [];
  #buffered = 0;
  #waiters = [];
  #error = null;

  static open(url = DEFAULT_URL) {
    return new Promise((resolve, reject) => {
      const socket = new WebSocket(url);
      socket.binaryType = "arraybuffer";
      socket.onopen = () => resolve(new Stream(socket));
      // A 403 (the user denied this site) surfaces as an error before open.
      socket.onerror = () => reject(new Error(`ios-webusb: could not connect to ${url} (not running, or access denied)`));
    });
  }

  constructor(socket) {
    this.#socket = socket;
    socket.onmessage = (event) => {
      const chunk = new Uint8Array(event.data);
      this.#chunks.push(chunk);
      this.#buffered += chunk.length;
      this.#wake();
    };
    socket.onclose = () => {
      this.#error ??= new Error("ios-webusb: connection closed");
      this.#wake();
    };
  }

  #wake() {
    for (const waiter of this.#waiters.splice(0)) waiter();
  }

  /** Reads exactly `length` bytes. */
  async read(length) {
    while (this.#buffered < length) {
      if (this.#error) throw this.#error;
      await new Promise((resolve) => this.#waiters.push(resolve));
    }
    const out = new Uint8Array(length);
    let offset = 0;
    while (offset < length) {
      const chunk = this.#chunks[0];
      const take = Math.min(chunk.length, length - offset);
      out.set(chunk.subarray(0, take), offset);
      offset += take;
      if (take === chunk.length) this.#chunks.shift();
      else this.#chunks[0] = chunk.subarray(take);
    }
    this.#buffered -= length;
    return out;
  }

  /** Reads whatever is buffered (at least one byte). */
  async readSome() {
    while (this.#buffered === 0) {
      if (this.#error) throw this.#error;
      await new Promise((resolve) => this.#waiters.push(resolve));
    }
    return this.read(this.#buffered);
  }

  write(bytes) {
    this.#socket.send(bytes);
  }

  close() {
    this.#socket.close();
  }
}

let nextTag = 1;

async function send(stream, message) {
  const body = encoder.encode(
    plist.build({
      ClientVersionString: "ios-webusb",
      ProgName: "ios-webusb",
      kLibUSBMuxVersion: 3,
      ...message,
    }),
  );
  const packet = new Uint8Array(HEADER_SIZE + body.length);
  const header = new DataView(packet.buffer);
  header.setUint32(0, packet.length, true);
  header.setUint32(4, VERSION_PLIST, true);
  header.setUint32(8, MESSAGE_PLIST, true);
  header.setUint32(12, nextTag++, true);
  packet.set(body, HEADER_SIZE);
  stream.write(packet);
}

async function receive(stream) {
  const header = new DataView((await stream.read(HEADER_SIZE)).buffer);
  const body = await stream.read(header.getUint32(0, true) - HEADER_SIZE);
  return plist.parse(decoder.decode(body));
}

function check(response) {
  if (response.MessageType === "Result" && response.Number !== 0) {
    throw new Error(`usbmuxd error ${response.Number}`);
  }
  return response;
}

function device(entry) {
  const properties = entry.Properties ?? entry;
  return {
    deviceId: entry.DeviceID ?? properties.DeviceID,
    udid: properties.SerialNumber,
    connectionType: properties.ConnectionType,
    properties,
  };
}

export class Usbmux {
  constructor(url = DEFAULT_URL) {
    this.url = url;
  }

  async #request(message) {
    const stream = await Stream.open(this.url);
    try {
      await send(stream, message);
      return check(await receive(stream));
    } finally {
      stream.close();
    }
  }

  /** Attached devices: [{ deviceId, udid, connectionType, properties }]. */
  async listDevices() {
    const response = await this.#request({ MessageType: "ListDevices" });
    return (response.DeviceList ?? []).map(device);
  }

  /** The host's lockdown pair record for a device (plist XML/binary bytes). */
  async readPairRecord(udid) {
    const response = await this.#request({ MessageType: "ReadPairRecord", PairRecordID: udid });
    return response.PairRecordData;
  }

  async readBuid() {
    return (await this.#request({ MessageType: "ReadBUID" })).BUID;
  }

  /** Calls onEvent({ type: "Attached" | "Detached", deviceId, ... }) until close(). */
  async listen(onEvent) {
    const stream = await Stream.open(this.url);
    await send(stream, { MessageType: "Listen" });
    check(await receive(stream));
    (async () => {
      for (;;) {
        const event = await receive(stream);
        onEvent({ type: event.MessageType, ...device(event) });
      }
    })().catch(() => {});
    return { close: () => stream.close() };
  }

  /** Opens a raw TCP tunnel to `port` on the device (default: lockdownd). */
  async connect(deviceId, port = LOCKDOWN_PORT) {
    const stream = await Stream.open(this.url);
    await send(stream, {
      MessageType: "Connect",
      DeviceID: deviceId,
      PortNumber: ((port & 0xff) << 8) | (port >> 8),
    });
    try {
      check(await receive(stream));
    } catch (error) {
      stream.close();
      throw error;
    }
    return stream;
  }
}
