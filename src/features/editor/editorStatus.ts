import { create } from 'zustand';

/** Cursor position shown in the status bar. */
export const useEditorStatus = create<{ line: number; column: number; set: (line: number, column: number) => void }>((set) => ({
  line: 1,
  column: 1,
  set: (line, column) => set({ line, column }),
}));
