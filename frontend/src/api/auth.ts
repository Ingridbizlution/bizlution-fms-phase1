import { apiFetch } from "./client";
import type { components } from "./schema";

export type TokenResponse = components["schemas"]["TokenResponse"];
export type CurrentUser = components["schemas"]["CurrentUser"];

export function loginWithPassword(input: {
  tenantCode: string;
  username: string;
  password: string;
}): Promise<TokenResponse> {
  return apiFetch<TokenResponse>("/auth/token", {
    method: "POST",
    auth: false,
    body: {
      grant_type: "password",
      tenant_code: input.tenantCode,
      username: input.username,
      password: input.password,
    },
  });
}

export function refreshAccessToken(refreshToken: string): Promise<TokenResponse> {
  return apiFetch<TokenResponse>("/auth/token/refresh", {
    method: "POST",
    auth: false,
    body: { refresh_token: refreshToken },
  });
}

export function fetchCurrentUser(): Promise<CurrentUser> {
  return apiFetch<CurrentUser>("/auth/me");
}

export function logoutSession(refreshToken: string): Promise<void> {
  return apiFetch<void>("/auth/logout", {
    method: "POST",
    body: { refresh_token: refreshToken },
  });
}
