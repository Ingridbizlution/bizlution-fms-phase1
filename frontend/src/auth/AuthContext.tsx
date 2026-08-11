import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { fetchCurrentUser, loginWithPassword, logoutSession, refreshAccessToken, type CurrentUser } from "../api/auth";
import { ApiError } from "../api/client";
import { clearSession, setOnSessionExpired, setRefreshHandler, setSession } from "../api/client";

const REFRESH_TOKEN_KEY = "fms.refresh_token";
const FACILITY_KEY = "fms.facility_id";

type AuthStatus = "loading" | "authenticated" | "anonymous";

interface AuthState {
  status: AuthStatus;
  currentUser: CurrentUser | null;
  facilityId: string | null;
  /** Set only on a failed login attempt, so the login form can show it. */
  loginError: ApiError | null;
}

interface AuthContextValue extends AuthState {
  login: (input: { tenantCode: string; username: string; password: string }) => Promise<void>;
  logout: () => Promise<void>;
  setFacilityId: (facilityId: string) => void;
}

const AuthContext = createContext<AuthContextValue | null>(null);

let refreshToken: string | null = sessionStorage.getItem(REFRESH_TOKEN_KEY);

function persistRefreshToken(token: string | null) {
  refreshToken = token;
  if (token) sessionStorage.setItem(REFRESH_TOKEN_KEY, token);
  else sessionStorage.removeItem(REFRESH_TOKEN_KEY);
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AuthState>({
    status: "loading",
    currentUser: null,
    facilityId: null,
    loginError: null,
  });

  const applySession = useCallback((currentUser: CurrentUser) => {
    const savedFacility = sessionStorage.getItem(FACILITY_KEY);
    const facilities = currentUser.accessible_facilities ?? [];
    const facilityId =
      (savedFacility && facilities.some((f) => f.id === savedFacility) ? savedFacility : facilities[0]?.id) ?? null;
    if (facilityId) {
      sessionStorage.setItem(FACILITY_KEY, facilityId);
      setSession({ facilityId });
    }
    setState({ status: "authenticated", currentUser, facilityId, loginError: null });
  }, []);

  const goAnonymous = useCallback(() => {
    persistRefreshToken(null);
    sessionStorage.removeItem(FACILITY_KEY);
    clearSession();
    setState({ status: "anonymous", currentUser: null, facilityId: null, loginError: null });
  }, []);

  const runRefresh = useCallback(async (): Promise<boolean> => {
    if (!refreshToken) return false;
    try {
      const tokenRes = await refreshAccessToken(refreshToken);
      setSession({ accessToken: tokenRes.access_token ?? null, tenantId: tokenRes.tenant_id ?? null });
      if (tokenRes.refresh_token) persistRefreshToken(tokenRes.refresh_token);
      return true;
    } catch {
      goAnonymous();
      return false;
    }
  }, [goAnonymous]);

  useEffect(() => {
    setRefreshHandler(runRefresh);
    setOnSessionExpired(goAnonymous);
    return () => {
      setRefreshHandler(null);
      setOnSessionExpired(null);
    };
  }, [runRefresh, goAnonymous]);

  useEffect(() => {
    (async () => {
      if (!refreshToken) {
        setState((s) => ({ ...s, status: "anonymous" }));
        return;
      }
      const ok = await runRefresh();
      if (!ok) return; // runRefresh already went anonymous
      try {
        const currentUser = await fetchCurrentUser();
        applySession(currentUser);
      } catch {
        goAnonymous();
      }
    })();
    // Runs once on mount only — bootstrapping the session from a stored refresh token.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const login = useCallback(
    async (input: { tenantCode: string; username: string; password: string }) => {
      setState((s) => ({ ...s, loginError: null }));
      try {
        const tokenRes = await loginWithPassword(input);
        setSession({ accessToken: tokenRes.access_token ?? null, tenantId: tokenRes.tenant_id ?? null });
        persistRefreshToken(tokenRes.refresh_token ?? null);
        const currentUser = await fetchCurrentUser();
        applySession(currentUser);
      } catch (err) {
        clearSession();
        setState((s) => ({ ...s, loginError: err instanceof ApiError ? err : new ApiError({ type: "about:blank", title: "Login failed", status: 0 }) }));
        throw err;
      }
    },
    [applySession],
  );

  const logout = useCallback(async () => {
    const tokenToRevoke = refreshToken;
    try {
      if (tokenToRevoke) await logoutSession(tokenToRevoke);
    } catch {
      // Best-effort: /auth/logout needs the still-live credentials to revoke the
      // right token — if that fails, local state is cleared below regardless.
    } finally {
      goAnonymous();
    }
  }, [goAnonymous]);

  const setFacilityId = useCallback((facilityId: string) => {
    sessionStorage.setItem(FACILITY_KEY, facilityId);
    setSession({ facilityId });
    setState((s) => ({ ...s, facilityId }));
  }, []);

  const value = useMemo<AuthContextValue>(
    () => ({ ...state, login, logout, setFacilityId }),
    [state, login, logout, setFacilityId],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within an AuthProvider");
  return ctx;
}
