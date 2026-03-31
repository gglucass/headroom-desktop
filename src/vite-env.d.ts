/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly HEADROOM_ACCOUNT_API_BASE_URL?: string;
  readonly VITE_HEADROOM_SALES_CONTACT_URL?: string;
  readonly VITE_HEADROOM_CONTACT_FORM_URL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
