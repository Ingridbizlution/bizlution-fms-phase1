import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { DndContext, PointerSensor, useDraggable, useDroppable, useSensor, useSensors, type DragEndEvent } from "@dnd-kit/core";
import { CSS } from "@dnd-kit/utilities";
import { listAvailableActions, listWorkOrders, listWorkOrderStatuses, transitionWorkOrder, type AvailableAction, type WorkOrder } from "../../api/workOrders";
import { ApiError } from "../../api/client";
import { Can } from "../../auth/Can";
import { useAuth } from "../../auth/AuthContext";
import { humanizeEnum } from "../../lib/format";
import { priorityBadge, slaStateBadge, workOrderCategoryBadge } from "../../lib/statusColors";
import { useCursorList } from "../../lib/useCursorList";
import { EmptyState } from "../../shell/EmptyState";
import { LoadMore } from "../../shell/LoadMore";
import { PageBody } from "../../shell/PageBody";
import { PageHeader } from "../../shell/PageHeader";

const COLUMNS: { key: string; labelKey: string }[] = [
  { key: "OPEN", labelKey: "workOrders.colOpen" },
  { key: "IN_PROGRESS", labelKey: "workOrders.colInProgress" },
  { key: "WAITING", labelKey: "workOrders.colWaiting" },
  { key: "TERMINAL", labelKey: "workOrders.colDone" },
];

function WorkOrderCardBody({ wo }: { wo: WorkOrder }) {
  return (
    <div className="card-body p-2">
      <div className="d-flex justify-content-between align-items-start">
        <code className="small text-secondary">{wo.wo_no}</code>
        <span className={`badge ${priorityBadge(wo.priority)}`}>{wo.priority}</span>
      </div>
      <div className="fw-medium">{wo.title}</div>
      {wo.asset && <div className="text-secondary small">{wo.asset.name}</div>}
      <span className={`badge ${slaStateBadge(wo.sla_state)} mt-1`}>{humanizeEnum(wo.sla_state) || "—"}</span>
    </div>
  );
}

/** Draggable card — a plain click (no pointer movement past the activation distance) still navigates;
 *  dnd-kit only takes over once the pointer has moved, so drag and click-to-open don't conflict. */
function DraggableWorkOrderCard({ wo, disabled, dragDisabled }: { wo: WorkOrder; disabled: boolean; dragDisabled: boolean }) {
  const navigate = useNavigate();
  const { attributes, listeners, setNodeRef, transform, isDragging } = useDraggable({ id: wo.id!, data: { wo }, disabled: disabled || dragDisabled });
  const style = { transform: CSS.Translate.toString(transform), opacity: isDragging ? 0.4 : disabled ? 0.6 : 1, cursor: disabled || dragDisabled ? "wait" : "grab" };
  return (
    <div
      ref={setNodeRef}
      {...listeners}
      {...attributes}
      style={style}
      className="card card-sm mb-2"
      onClick={() => !isDragging && navigate(`/work-orders/${wo.id}`)}
    >
      <WorkOrderCardBody wo={wo} />
    </div>
  );
}

function DroppableColumn({ id, label, children }: { id: string; label: string; children: React.ReactNode }) {
  const { setNodeRef, isOver } = useDroppable({ id });
  return (
    <div ref={setNodeRef} className="col-md-3">
      <div className={`badge ${workOrderCategoryBadge(id)} mb-2`}>{label}</div>
      <div className="rounded p-1" style={{ minHeight: 60, backgroundColor: isOver ? "var(--tblr-bg-surface-tertiary)" : "transparent", transition: "background-color 120ms" }}>
        {children}
      </div>
    </div>
  );
}

function KanbanBoard({ facilityId, priority, mine, onSwitchToTable }: { facilityId: string | null; priority: string; mine: boolean; onSwitchToTable: () => void }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [dragError, setDragError] = useState<string | null>(null);
  const [pendingWoId, setPendingWoId] = useState<string | null>(null);
  const [choiceModal, setChoiceModal] = useState<{ wo: WorkOrder; actions: AvailableAction[] } | null>(null);
  const sensors = useSensors(useSensor(PointerSensor, { activationConstraint: { distance: 8 } }));

  const queryKey = ["work-orders-kanban", facilityId, priority, mine];
  const { data, isLoading } = useQuery({
    queryKey,
    queryFn: () => listWorkOrders({ facilityId: facilityId ?? undefined, priority: priority || undefined, mine, limit: 200 }),
    enabled: !!facilityId,
  });
  const statusesQuery = useQuery({ queryKey: ["work-order-statuses"], queryFn: listWorkOrderStatuses });
  const items = data?.data ?? [];

  const transitionMutation = useMutation({
    mutationFn: ({ wo, action }: { wo: WorkOrder; action: string }) => transitionWorkOrder(wo.id!, wo.version!, { action }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey }),
    onError: (err) => setDragError(err instanceof ApiError ? err.problem.detail ?? err.message : t("workOrders.detail.actionFailed")),
    onSettled: () => setPendingWoId(null),
  });

  async function handleDragEnd(event: DragEndEvent) {
    const wo = event.active.data.current?.wo as WorkOrder | undefined;
    const targetCategory = event.over?.id as string | undefined;
    if (!wo || !targetCategory || targetCategory === wo.status_category) return;

    setDragError(null);
    setPendingWoId(wo.id!);
    try {
      const [actionsRes] = await Promise.all([listAvailableActions(wo.id!)]);
      const statusToCategory = new Map((statusesQuery.data?.data ?? []).map((s) => [s.code, s.category]));
      const candidates = (actionsRes.data ?? []).filter((a) => a.permitted !== false && a.to_status && statusToCategory.get(a.to_status) === targetCategory);

      if (candidates.length === 0) {
        setDragError(t("workOrders.dragInvalidTransition"));
        setPendingWoId(null);
        return;
      }
      if (candidates.every((a) => (a.required_fields?.length ?? 0) > 0)) {
        setPendingWoId(null);
        navigate(`/work-orders/${wo.id}`);
        return;
      }
      const simple = candidates.filter((a) => (a.required_fields?.length ?? 0) === 0);
      if (simple.length === 1) {
        transitionMutation.mutate({ wo, action: simple[0].action! });
        return;
      }
      setPendingWoId(null);
      setChoiceModal({ wo, actions: candidates });
    } catch (err) {
      setPendingWoId(null);
      setDragError(err instanceof ApiError ? err.problem.detail ?? err.message : t("workOrders.detail.actionFailed"));
    }
  }

  function chooseAction(action: AvailableAction) {
    if (!choiceModal) return;
    if ((action.required_fields?.length ?? 0) > 0) {
      navigate(`/work-orders/${choiceModal.wo.id}`);
    } else {
      setPendingWoId(choiceModal.wo.id!);
      transitionMutation.mutate({ wo: choiceModal.wo, action: action.action! });
    }
    setChoiceModal(null);
  }

  if (isLoading) {
    return (
      <div className="d-flex justify-content-center py-5">
        <div className="spinner-border text-primary" role="status" aria-label={t("workOrders.loadingWorkOrders")} />
      </div>
    );
  }

  return (
    <>
      {data?.page?.next_cursor && (
        <div className="alert alert-warning d-flex justify-content-between align-items-center" role="alert">
          {t("workOrders.kanbanTruncated", { count: items.length })}
          <button type="button" className="btn btn-sm btn-outline-secondary" onClick={onSwitchToTable}>
            {t("workOrders.table")}
          </button>
        </div>
      )}
      {dragError && (
        <div className="alert alert-danger d-flex justify-content-between align-items-center" role="alert">
          {dragError}
          <button type="button" className="btn-close" aria-label={t("common.close")} onClick={() => setDragError(null)} />
        </div>
      )}
      <DndContext sensors={sensors} onDragEnd={handleDragEnd}>
        <div className="row g-3">
          {COLUMNS.map((col) => {
            const colItems = items.filter((wo) => wo.status_category === col.key);
            return (
              <DroppableColumn id={col.key} label={`${t(col.labelKey)} (${colItems.length})`} key={col.key}>
                {colItems.map((wo) => (
                  <DraggableWorkOrderCard wo={wo} key={wo.id} disabled={pendingWoId === wo.id} dragDisabled={!statusesQuery.data} />
                ))}
                {colItems.length === 0 && <p className="text-secondary small">{t("workOrders.nothingHere")}</p>}
              </DroppableColumn>
            );
          })}
        </div>
      </DndContext>

      {choiceModal && (
        <div className="modal modal-blur show d-block" role="dialog" style={{ backgroundColor: "rgba(0,0,0,0.25)" }}>
          <div className="modal-dialog modal-dialog-centered" role="document">
            <div className="modal-content">
              <div className="modal-header">
                <h5 className="modal-title">{t("workOrders.dragChooseAction")}</h5>
                <button type="button" className="btn-close" aria-label={t("common.close")} onClick={() => setChoiceModal(null)} />
              </div>
              <div className="modal-body d-flex flex-column gap-2">
                {choiceModal.actions.map((action) => (
                  <button key={action.action} type="button" className="btn btn-outline-primary text-start" onClick={() => chooseAction(action)}>
                    {action.label_zh ?? action.action}
                  </button>
                ))}
              </div>
            </div>
          </div>
        </div>
      )}
    </>
  );
}

function TableView({ facilityId, priority, mine }: { facilityId: string | null; priority: string; mine: boolean }) {
  const { t } = useTranslation();
  const { items, isLoading, hasNextPage, isFetchingNextPage, fetchNextPage } = useCursorList(
    ["work-orders-table", facilityId, priority, mine],
    (cursor) => listWorkOrders({ facilityId: facilityId ?? undefined, priority: priority || undefined, mine, cursor }),
    { enabled: !!facilityId },
  );

  return (
    <div className="card">
      <div className="table-responsive">
        <table className="table table-vcenter card-table">
          <thead>
            <tr>
              <th>{t("workOrders.colWoNo")}</th>
              <th>{t("workOrders.colTitle")}</th>
              <th>{t("workOrders.colStatus")}</th>
              <th>{t("workOrders.colPriority")}</th>
              <th>{t("workOrders.colSla")}</th>
              <th>{t("workOrders.colAssignee")}</th>
            </tr>
          </thead>
          <tbody>
            {items.map((wo) => (
              <tr key={wo.id}>
                <td>
                  <Link to={`/work-orders/${wo.id}`}>
                    <code>{wo.wo_no}</code>
                  </Link>
                </td>
                <td>{wo.title}</td>
                <td>
                  <span className={`badge ${workOrderCategoryBadge(wo.status_category)}`}>{humanizeEnum(wo.status)}</span>
                </td>
                <td>
                  <span className={`badge ${priorityBadge(wo.priority)}`}>{wo.priority}</span>
                </td>
                <td>
                  <span className={`badge ${slaStateBadge(wo.sla_state)}`}>{humanizeEnum(wo.sla_state) || "—"}</span>
                </td>
                <td>{wo.assignee?.display_name ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!isLoading && items.length === 0 && <EmptyState title={t("workOrders.noWorkOrdersMatch")} />}
      <LoadMore hasMore={!!hasNextPage} loading={isFetchingNextPage} onClick={() => fetchNextPage()} />
    </div>
  );
}

export function WorkOrdersBoardPage() {
  const { t } = useTranslation();
  const { facilityId } = useAuth();
  const [view, setView] = useState<"kanban" | "table">("kanban");
  const [priority, setPriority] = useState("");
  const [mine, setMine] = useState(false);

  return (
    <>
      <PageHeader
        title={t("workOrders.title")}
        actions={
          <Can permission="work_order:create">
            <Link to="/work-orders/new" className="btn btn-primary">
              {t("workOrders.newWorkOrder")}
            </Link>
          </Can>
        }
      />
      <PageBody>
        <div className="d-flex flex-wrap gap-2 mb-3 align-items-center">
          <div className="btn-group">
            <button type="button" className={`btn btn-sm ${view === "kanban" ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setView("kanban")}>
              {t("workOrders.board")}
            </button>
            <button type="button" className={`btn btn-sm ${view === "table" ? "btn-primary" : "btn-outline-primary"}`} onClick={() => setView("table")}>
              {t("workOrders.table")}
            </button>
          </div>
          <select className="form-select form-select-sm w-auto" value={priority} onChange={(e) => setPriority(e.target.value)}>
            <option value="">{t("workOrders.allPriorities")}</option>
            <option value="URGENT">{t("workOrders.priorityUrgent")}</option>
            <option value="HIGH">{t("workOrders.priorityHigh")}</option>
            <option value="MEDIUM">{t("workOrders.priorityMedium")}</option>
            <option value="LOW">{t("workOrders.priorityLow")}</option>
          </select>
          <label className="form-check form-check-inline ms-1">
            <input type="checkbox" className="form-check-input" checked={mine} onChange={(e) => setMine(e.target.checked)} />
            <span className="form-check-label">{t("workOrders.mineOnly")}</span>
          </label>
        </div>

        {view === "kanban" ? (
          <KanbanBoard facilityId={facilityId} priority={priority} mine={mine} onSwitchToTable={() => setView("table")} />
        ) : (
          <TableView facilityId={facilityId} priority={priority} mine={mine} />
        )}
      </PageBody>
    </>
  );
}
