/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_HEADROOM_POLAR_PRO_CHECKOUT_URL?: string;
  readonly VITE_HEADROOM_SALES_CONTACT_URL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
