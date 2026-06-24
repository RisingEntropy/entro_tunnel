/// <reference types="vite/client" />

// Importing an .svg yields its asset URL (Vite emits the file and returns a path).
declare module "*.svg" {
  const src: string;
  export default src;
}
