import { useTranslation } from "react-i18next";

interface LoadMoreProps {
  hasMore: boolean;
  loading: boolean;
  onClick: () => void;
}

export function LoadMore({ hasMore, loading, onClick }: LoadMoreProps) {
  const { t } = useTranslation();
  if (!hasMore) return null;
  return (
    <div className="d-flex justify-content-center py-3">
      <button type="button" className="btn btn-outline-secondary" onClick={onClick} disabled={loading}>
        {loading ? t("common.loading") : t("common.loadMore")}
      </button>
    </div>
  );
}
