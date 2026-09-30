import { api } from "../../lib/api";

export interface User {
  id: string;
  email: string;
  created_at: string;
}

export interface TokenPair {
  access_token: string;
  refresh_token: string;
  expires_in: number;
}

/** Register answer: either a user, or "confirm your email first" (202). */
export interface RegisterResult {
  user: User | null;
  needs_confirmation?: boolean;
  message?: string;
}

export const authApi = {
  register: (email: string, password: string) =>
    api<RegisterResult>("/v1/auth/register", {
      method: "POST",
      body: { email, password },
    }),
  login: (email: string, password: string) =>
    api<{ user: User; tokens: TokenPair }>("/v1/auth/login", {
      method: "POST",
      body: { email, password },
    }),
  refresh: (refresh_token: string) =>
    api<{ user: User; tokens: TokenPair }>("/v1/auth/refresh", {
      method: "POST",
      body: { refresh_token },
    }),
  me: (token: string) => api<{ user: User }>("/v1/auth/me", { token }),
  logout: (refresh_token: string) =>
    api<{ ok: boolean }>("/v1/auth/logout", {
      method: "POST",
      body: { refresh_token },
    }),
  deleteAccount: (token: string) =>
    api<{ ok: boolean }>("/v1/auth/account", { method: "DELETE", token }),
};
