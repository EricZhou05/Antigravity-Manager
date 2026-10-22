import React from 'react';
import { AlertTriangle, Hourglass, Activity, ShieldAlert } from 'lucide-react';
import { useTranslation } from 'react-i18next';

interface QoSStatusBadgeProps {
    status?: 'suspicious' | 'jailed' | 'probing' | 'half_open' | null;
    size?: 'xs' | 'sm' | 'md';
}

export const QoSStatusBadge: React.FC<QoSStatusBadgeProps> = ({ status, size = 'sm' }) => {
    const { t } = useTranslation();

    if (!status) return null;

    const sizeClass =
        size === 'xs'
            ? 'text-[8px] leading-tight px-1 py-0.5'
            : size === 'md'
                ? 'text-[10px] px-2 py-0.5'
                : 'text-[9px] px-1.5 py-0.5';

    const iconClass = size === 'xs' ? 'w-2 h-2 shrink-0' : 'w-2.5 h-2.5 shrink-0';

    switch (status) {
        case 'suspicious':
            return (
                <span
                    className={`flex items-center gap-0.5 rounded-md bg-yellow-500/15 text-yellow-600 dark:text-yellow-400 font-bold shadow-sm border border-yellow-500/30 shrink-0 ${sizeClass}`}
                    title={t('accounts.qos.suspicious_hint')}
                >
                    <AlertTriangle className={iconClass} />
                    <span>{t('accounts.qos.suspicious')}</span>
                </span>
            );
        case 'jailed':
            return (
                <span
                    className={`flex items-center gap-0.5 rounded-md bg-yellow-500/15 text-yellow-600 dark:text-yellow-400 font-bold shadow-sm border border-yellow-500/30 shrink-0 ${sizeClass}`}
                    title={t('accounts.qos.jailed_hint')}
                >
                    <Hourglass className={iconClass} />
                    <span>{t('accounts.qos.jailed')}</span>
                </span>
            );
        case 'probing':
            return (
                <span
                    className={`flex items-center gap-0.5 rounded-md bg-yellow-500/15 text-yellow-600 dark:text-yellow-400 font-bold shadow-sm border border-yellow-500/30 shrink-0 ${sizeClass}`}
                    title={t('accounts.qos.probing_hint')}
                >
                    <Activity className={`${iconClass} animate-pulse`} />
                    <span>{t('accounts.qos.probing')}</span>
                </span>
            );
        case 'half_open':
            return (
                <span
                    className={`flex items-center gap-0.5 rounded-md bg-yellow-500/15 text-yellow-600 dark:text-yellow-400 font-bold shadow-sm border border-yellow-500/30 shrink-0 ${sizeClass}`}
                    title={t('accounts.qos.half_open_hint')}
                >
                    <ShieldAlert className={iconClass} />
                    <span>{t('accounts.qos.half_open')}</span>
                </span>
            );
        default:
            return null;
    }
};
