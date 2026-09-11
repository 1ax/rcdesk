import type { SignalMessage } from "./generated/SignalMessage";
import { formatClientVersion } from "./version";

const hello: SignalMessage = {
  type: "hello",
  role: "client",
  version: __APP_VERSION__,
};

console.log(hello);

document.querySelector<HTMLDivElement>("#app")!.textContent = formatClientVersion(
  __APP_VERSION__,
);
