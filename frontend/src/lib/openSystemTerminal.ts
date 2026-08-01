import { getTauriInvoke } from "./tauriBridge";

/**
 * Opens a new system terminal window on the desktop app.
 *
 * The `open_system_terminal` Tauri command takes no arguments: it never
 * receives commands to execute, so nothing is run on the user's behalf.
 * Commands are delivered through the clipboard instead; the user pastes
 * and presses Enter themselves. Web/NPX builds cannot open a desktop
 * terminal and always report false.
 */
export const canOpenSystemTerminal = (): boolean => getTauriInvoke() !== null;

export const openSystemTerminal = async (): Promise<boolean> => {
  const invoke = getTauriInvoke();
  if (!invoke) return false;
  try {
    await invoke("open_system_terminal");
    return true;
  } catch (error) {
    console.error("Failed to open the system terminal.", error);
    return false;
  }
};
