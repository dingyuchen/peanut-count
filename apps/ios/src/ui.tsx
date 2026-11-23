import type { ReactNode } from 'react';
import {
  ActivityIndicator, Pressable, StyleSheet, Text, TextInput, View, useColorScheme,
  type StyleProp, type TextInputProps, type TextStyle, type ViewStyle,
} from 'react-native';

type PaletteName = 'light' | 'dark';

const palettes = {
  light: {
    background: '#F4F5F0',
    surface: '#FFFFFF',
    surfaceMuted: '#ECEFE8',
    border: '#DCE1D6',
    text: '#16231A',
    muted: '#5B685E',
    faint: '#8A948C',
    accent: '#1E5A35',
    onAccent: '#FFFFFF',
    accentSoft: '#E1EDE3',
    chartMuted: '#A9C2AF',
    positive: '#1C7442',
    danger: '#B3261E',
    warningSurface: '#FFF3DC',
    warningText: '#6E4600',
    errorSurface: '#FCE8E6',
    successSurface: '#E1F0E5',
  },
  dark: {
    background: '#0E1310',
    surface: '#171F1A',
    surfaceMuted: '#202A23',
    border: '#2B362E',
    text: '#E8EEE9',
    muted: '#A0ACA3',
    faint: '#6F7C73',
    accent: '#7BD69A',
    onAccent: '#0A1A0F',
    accentSoft: '#1D3324',
    chartMuted: '#4F6F58',
    positive: '#7BD69A',
    danger: '#F2B8B5',
    warningSurface: '#3A2D10',
    warningText: '#F4CD80',
    errorSurface: '#3B1A18',
    successSurface: '#173022',
  },
} satisfies Record<PaletteName, Record<string, string>>;

export type Palette = typeof palettes.light;

function createStyles(c: Palette) {
  return StyleSheet.create({
    screen: { flex: 1, backgroundColor: c.background },
    content: { paddingHorizontal: 20, paddingBottom: 48, gap: 16 },
    navBar: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', minHeight: 44, paddingHorizontal: 12 },
    navTitle: { fontSize: 17, fontWeight: '600', color: c.text },
    navSide: { minWidth: 72 },
    eyebrow: { fontSize: 13, fontWeight: '600', letterSpacing: 0.4, textTransform: 'uppercase', color: c.muted },
    largeTitle: { fontSize: 34, fontWeight: '800', letterSpacing: -0.5, color: c.text },
    title: { fontSize: 22, fontWeight: '700', color: c.text },
    headline: { fontSize: 17, fontWeight: '600', color: c.text },
    body: { fontSize: 16, lineHeight: 22, color: c.text },
    secondary: { fontSize: 15, lineHeight: 20, color: c.muted },
    footnote: { fontSize: 13, lineHeight: 18, color: c.muted },
    sectionLabel: { fontSize: 13, fontWeight: '600', letterSpacing: 0.4, textTransform: 'uppercase', color: c.muted, marginTop: 8, marginLeft: 4 },
    card: { backgroundColor: c.surface, borderRadius: 16, padding: 16, gap: 12, borderWidth: StyleSheet.hairlineWidth, borderColor: c.border },
    row: { flexDirection: 'row', alignItems: 'center', gap: 12 },
    divider: { height: StyleSheet.hairlineWidth, backgroundColor: c.border },
    amount: { fontSize: 16, fontWeight: '600', fontVariant: ['tabular-nums'], color: c.text, textAlign: 'right' },
    field: { gap: 6 },
    fieldLabel: { fontSize: 14, fontWeight: '600', color: c.text },
    input: {
      minHeight: 48, borderRadius: 12, paddingHorizontal: 14, fontSize: 16,
      color: c.text, backgroundColor: c.surfaceMuted, borderWidth: 1, borderColor: 'transparent',
    },
    inputInvalid: { borderColor: c.danger },
    button: { minHeight: 50, borderRadius: 14, paddingHorizontal: 18, alignItems: 'center', justifyContent: 'center', flexDirection: 'row', gap: 8 },
    buttonLabel: { fontSize: 17, fontWeight: '600' },
    pressed: { opacity: 0.7 },
    disabled: { opacity: 0.45 },
    pill: { flexDirection: 'row', alignItems: 'center', gap: 6, alignSelf: 'flex-start', borderRadius: 999, paddingHorizontal: 10, paddingVertical: 5 },
    pillDot: { width: 7, height: 7, borderRadius: 4 },
    pillLabel: { fontSize: 13, fontWeight: '600' },
    chips: { flexDirection: 'row', flexWrap: 'wrap', gap: 8 },
    chip: { borderRadius: 999, paddingHorizontal: 12, paddingVertical: 7, backgroundColor: c.accentSoft },
    chipLabel: { fontSize: 14, fontWeight: '500', color: c.accent },
    segmented: { flexDirection: 'row', backgroundColor: c.surfaceMuted, borderRadius: 12, padding: 3 },
    segment: { flex: 1, minHeight: 38, borderRadius: 9, alignItems: 'center', justifyContent: 'center', paddingHorizontal: 8 },
    segmentActive: { backgroundColor: c.surface, shadowColor: '#000', shadowOpacity: 0.08, shadowRadius: 4, shadowOffset: { width: 0, height: 1 } },
    segmentLabel: { fontSize: 14, fontWeight: '600', color: c.muted },
    segmentLabelActive: { color: c.text },
    banner: { flexDirection: 'row', alignItems: 'flex-start', gap: 10, borderRadius: 14, padding: 14 },
    bannerText: { flex: 1, fontSize: 15, lineHeight: 20 },
    stat: { flex: 1, gap: 2 },
    statValue: { fontSize: 22, fontWeight: '700', fontVariant: ['tabular-nums'], color: c.text },
    statLabel: { fontSize: 13, color: c.muted },
    bottomBar: { paddingHorizontal: 20, paddingTop: 12, paddingBottom: 8, gap: 8, backgroundColor: c.background, borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: c.border },
  });
}

export interface Theme {
  c: Palette;
  s: ReturnType<typeof createStyles>;
  dark: boolean;
}

const themes: Record<PaletteName, Theme> = {
  light: { c: palettes.light, s: createStyles(palettes.light), dark: false },
  dark: { c: palettes.dark, s: createStyles(palettes.dark), dark: true },
};

export function useTheme(): Theme {
  const scheme = useColorScheme();
  return themes[scheme === 'dark' ? 'dark' : 'light'];
}

type ButtonVariant = 'primary' | 'secondary' | 'plain' | 'destructive';

export function Button({ label, onPress, variant = 'primary', disabled, loading, compact }: {
  label: string;
  onPress?: () => void;
  variant?: ButtonVariant;
  disabled?: boolean;
  loading?: boolean;
  compact?: boolean;
}) {
  const { c, s } = useTheme();
  const tones: Record<ButtonVariant, [string, string]> = {
    primary: [c.accent, c.onAccent],
    secondary: [c.accentSoft, c.accent],
    plain: ['transparent', c.accent],
    destructive: [c.errorSurface, c.danger],
  };
  const [background, foreground] = tones[variant];
  return <Pressable
    accessibilityRole="button"
    accessibilityState={{ disabled: !!(disabled || loading), busy: !!loading }}
    disabled={disabled || loading}
    onPress={onPress}
    hitSlop={compact ? 8 : 0}
    style={({ pressed }) => [
      s.button, { backgroundColor: background },
      compact && { minHeight: 36, paddingHorizontal: 8 },
      pressed && s.pressed, disabled && s.disabled,
    ]}>
    {loading && <ActivityIndicator color={foreground} />}
    <Text style={[s.buttonLabel, { color: foreground }, compact && { fontSize: 17, fontWeight: variant === 'plain' ? '400' : '600' }]}>{label}</Text>
  </Pressable>;
}

export function NavBar({ left, title, right }: { left?: ReactNode; title: string; right?: ReactNode }) {
  const { s } = useTheme();
  return <View style={s.navBar}>
    <View style={[s.navSide, { alignItems: 'flex-start' }]}>{left}</View>
    <Text style={s.navTitle} numberOfLines={1} accessibilityRole="header">{title}</Text>
    <View style={[s.navSide, { alignItems: 'flex-end' }]}>{right}</View>
  </View>;
}

export function Field({ label, hint, invalid, autoCapitalize = 'none', style, ...properties }: TextInputProps & {
  label?: string;
  hint?: string;
  invalid?: boolean;
  style?: StyleProp<ViewStyle>;
}) {
  const { c, s } = useTheme();
  return <View style={[s.field, style]}>
    {label && <Text style={s.fieldLabel}>{label}</Text>}
    <TextInput accessibilityLabel={label ?? properties.placeholder} autoCapitalize={autoCapitalize} autoCorrect={false}
      placeholderTextColor={c.faint} style={[s.input, invalid && s.inputInvalid]} {...properties} />
    {hint && <Text style={[s.footnote, invalid && { color: c.danger }]}>{hint}</Text>}
  </View>;
}

export function Segmented<T extends string>({ options, value, onChange, label }: {
  options: readonly { value: T; label: string }[];
  value: T;
  onChange: (value: T) => void;
  label?: string;
}) {
  const { s } = useTheme();
  return <View style={s.field}>
    {label && <Text style={s.fieldLabel}>{label}</Text>}
    <View style={s.segmented} accessibilityRole="tablist">
      {options.map((option) => {
        const active = option.value === value;
        return <Pressable key={option.value} accessibilityRole="tab" accessibilityState={{ selected: active }}
          onPress={() => onChange(option.value)} style={[s.segment, active && s.segmentActive]}>
          <Text style={[s.segmentLabel, active && s.segmentLabelActive]}>{option.label}</Text>
        </Pressable>;
      })}
    </View>
  </View>;
}

export function Chips({ values, onSelect }: { values: readonly string[]; onSelect: (value: string) => void }) {
  const { s } = useTheme();
  if (!values.length) return null;
  return <View style={s.chips}>
    {values.map((value) => <Pressable key={value} accessibilityRole="button" accessibilityLabel={`Use ${value}`}
      onPress={() => onSelect(value)} style={({ pressed }) => [s.chip, pressed && s.pressed]}>
      <Text style={s.chipLabel}>{value}</Text>
    </Pressable>)}
  </View>;
}

export function Pill({ label, tone = 'accent' }: { label: string; tone?: 'accent' | 'muted' }) {
  const { c, s } = useTheme();
  const [background, foreground] = tone === 'muted' ? [c.surfaceMuted, c.muted] : [c.accentSoft, c.accent];
  return <View style={[s.pill, { backgroundColor: background }]}>
    <View style={[s.pillDot, { backgroundColor: foreground }]} />
    <Text style={[s.pillLabel, { color: foreground }]}>{label}</Text>
  </View>;
}

export type BannerTone = 'info' | 'warning' | 'error' | 'success';

export function Banner({ tone = 'info', text, onDismiss, children }: {
  tone?: BannerTone;
  text: string;
  onDismiss?: () => void;
  children?: ReactNode;
}) {
  const { c, s } = useTheme();
  const tones: Record<BannerTone, [string, string]> = {
    info: [c.surfaceMuted, c.text],
    warning: [c.warningSurface, c.warningText],
    error: [c.errorSurface, c.danger],
    success: [c.successSurface, c.positive],
  };
  const [background, foreground] = tones[tone];
  return <View style={[s.banner, { backgroundColor: background }]} accessibilityLiveRegion="polite">
    <View style={{ flex: 1, gap: 10 }}>
      <Text style={[s.bannerText, { color: foreground }]}>{text}</Text>
      {children}
    </View>
    {onDismiss && <Pressable accessibilityRole="button" accessibilityLabel="Dismiss" hitSlop={12} onPress={onDismiss}>
      <Text style={[s.bannerText, { color: foreground, flex: 0, fontWeight: '600' }]}>✕</Text>
    </Pressable>}
  </View>;
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const LONG_MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];

export const shortDate = (date: string): string => `${MONTHS[Number(date.slice(5, 7)) - 1]} ${Number(date.slice(8, 10))}`;
export const monthTitle = (date: string): string => `${LONG_MONTHS[Number(date.slice(5, 7)) - 1]} ${date.slice(0, 4)}`;

// Amounts are exact decimal strings with two fraction digits ("-1250.00").
export const toCents = (amount: string): bigint => BigInt(amount.replace('.', ''));

export function formatCents(cents: bigint, { signed = false }: { signed?: boolean } = {}): string {
  const negative = cents < 0n;
  const digits = (negative ? -cents : cents).toString().padStart(3, '0');
  const whole = digits.slice(0, -2).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  const sign = negative ? '−' : signed && cents > 0n ? '+' : '';
  return `${sign}${whole}.${digits.slice(-2)}`;
}

export const formatAmount = (amount: string): string => formatCents(toCents(amount), { signed: true });

// An amount with price-tag cents: the cents are set smaller and aligned to the top of
// the whole units, without a decimal point. Screen readers get the full amount.
export function Money({ cents, size = 17, weight = '700', color, unit }: {
  cents: bigint;
  size?: number;
  weight?: TextStyle['fontWeight'];
  color?: string;
  unit?: string;
}) {
  const { c } = useTheme();
  const text = formatCents(cents);
  const ink = color ?? c.text;
  const small = Math.round(size * 0.52);
  return <View style={{ flexDirection: 'row', alignItems: 'flex-start' }} accessible
    accessibilityLabel={unit ? `${text} ${unit}` : text}>
    <Text style={{ fontSize: size, lineHeight: Math.round(size * 1.2), fontWeight: weight, color: ink, fontVariant: ['tabular-nums'] }}>
      {text.slice(0, -3)}
    </Text>
    <Text style={{
      fontSize: small, lineHeight: small, fontWeight: weight, color: ink, fontVariant: ['tabular-nums'],
      marginTop: Math.round(size * 0.225), marginLeft: Math.max(1, Math.round(size * 0.04)),
    }}>{text.slice(-2)}</Text>
  </View>;
}
