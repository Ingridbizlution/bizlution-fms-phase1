import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Navigate, Outlet, Route, Routes } from "react-router-dom";
import { AuthProvider } from "./auth/AuthContext";
import { ProtectedRoute } from "./auth/ProtectedRoute";
import { AppShell } from "./shell/AppShell";
import { DashboardPage } from "./routes/DashboardPage";
import { LoginPage } from "./routes/LoginPage";
import { AssetCreatePage } from "./routes/assets/AssetCreatePage";
import { AssetDetailPage } from "./routes/assets/AssetDetailPage";
import { AssetModelsPage } from "./routes/assets/AssetModelsPage";
import { AssetsListPage } from "./routes/assets/AssetsListPage";
import { WorkOrderCreatePage } from "./routes/work-orders/WorkOrderCreatePage";
import { WorkOrderDetailPage } from "./routes/work-orders/WorkOrderDetailPage";
import { WorkOrdersBoardPage } from "./routes/work-orders/WorkOrdersBoardPage";
import { BookingPage } from "./routes/reservations/BookingPage";
import { ReservationDetailPage } from "./routes/reservations/ReservationDetailPage";
import { ReservationsListPage } from "./routes/reservations/ReservationsListPage";
import { FacilitiesPage } from "./routes/facilities/FacilitiesPage";
import { MaintenancePage } from "./routes/maintenance/MaintenancePage";
import { IotPage } from "./routes/iot/IotPage";
import { ServiceCataloguePage } from "./routes/catalogue/ServiceCataloguePage";
import { AdminPage } from "./routes/admin/AdminPage";
import { ReportingPage } from "./routes/reports/ReportingPage";

const queryClient = new QueryClient();

function AuthenticatedLayout() {
  return (
    <ProtectedRoute>
      <AppShell>
        <Outlet />
      </AppShell>
    </ProtectedRoute>
  );
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <AuthProvider>
          <Routes>
            <Route path="/login" element={<LoginPage />} />
            <Route element={<AuthenticatedLayout />}>
              <Route path="/" element={<DashboardPage />} />

              <Route path="/assets" element={<AssetsListPage />} />
              <Route path="/assets/new" element={<AssetCreatePage />} />
              <Route path="/asset-models" element={<AssetModelsPage />} />
              <Route path="/assets/:assetId" element={<AssetDetailPage />} />

              <Route path="/work-orders" element={<WorkOrdersBoardPage />} />
              <Route path="/work-orders/new" element={<WorkOrderCreatePage />} />
              <Route path="/work-orders/:workOrderId" element={<WorkOrderDetailPage />} />

              <Route path="/reservations" element={<ReservationsListPage />} />
              <Route path="/reservations/book" element={<BookingPage />} />
              <Route path="/reservations/:reservationId" element={<ReservationDetailPage />} />

              <Route path="/facilities" element={<FacilitiesPage />} />
              <Route path="/maintenance" element={<MaintenancePage />} />
              <Route path="/catalogue" element={<ServiceCataloguePage />} />
              <Route path="/iot" element={<IotPage />} />
              <Route path="/admin" element={<AdminPage />} />
              <Route path="/reports" element={<ReportingPage />} />
            </Route>
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </AuthProvider>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
