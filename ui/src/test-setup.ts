import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// vitest 未开启 globals，@testing-library/react 的自动清理不会注册，显式挂载。
afterEach(() => {
  cleanup();
  delete window.__TAURI_INTERNALS__;
});
