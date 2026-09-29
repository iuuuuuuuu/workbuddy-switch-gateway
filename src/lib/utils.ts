import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * 展示字段安全化：只接受非空字符串，其余一律归一成 null。
 *
 * 后端各通道（Tauri / webui HTTP / 导入文件预览）理论上保证展示字段为
 * string|null，但历史上已两次因加密信封对象 `{ $wbEncrypted, envelope }`
 * 原样透传触发 React #31 白屏（issue #36 / #38 / #40）。凡把账号字段
 * （nickname / email / uid / note …）放进 `||` 回退链再渲染的地方，
 * 一律先用本函数规整，避免对象漏网后被当作 React 子节点。
 */
export function displayText(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value : null;
}