{{/* SPDX-License-Identifier: Apache-2.0 */}}
{{/*
Naming
*/}}
{{- define "ontele.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "ontele.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "ontele.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Name of a companion component: {{ include "ontele.component" (dict "root" . "name" "postgresql") }} */}}
{{- define "ontele.component" -}}
{{- printf "%s-%s" (include "ontele.fullname" .root) .name | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Labels
*/}}
{{- define "ontele.labels" -}}
helm.sh/chart: {{ include "ontele.chart" . }}
app.kubernetes.io/name: {{ include "ontele.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: ontele
{{- end }}

{{/* Selector labels for a component: {{ include "ontele.selectorLabels" (dict "root" . "component" "server") }} */}}
{{- define "ontele.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ontele.name" .root }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end }}

{{- define "ontele.componentLabels" -}}
{{ include "ontele.labels" .root }}
app.kubernetes.io/component: {{ .component }}
{{- end }}

{{- define "ontele.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ontele.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "ontele.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}

{{/*
Public URL pieces (used for oauth2-proxy redirect, Grafana root URL, Dex issuer)
*/}}
{{- define "ontele.host" -}}
{{- if .Values.ingress.enabled }}{{ .Values.ingress.host }}{{ else if or .Values.httpRoute.enabled .Values.gateway.enabled }}{{ .Values.httpRoute.host }}{{ else }}localhost{{ end }}
{{- end }}

{{- define "ontele.scheme" -}}
{{- if .Values.externalUrl }}{{ (urlParse .Values.externalUrl).scheme }}{{ else if and .Values.ingress.enabled .Values.ingress.tls.enabled }}https{{ else if and (or .Values.httpRoute.enabled .Values.gateway.enabled) .Values.httpRoute.tls }}https{{ else }}http{{ end }}
{{- end }}

{{- define "ontele.publicUrl" -}}
{{- if .Values.externalUrl }}{{ trimSuffix "/" .Values.externalUrl }}{{ else }}{{ include "ontele.scheme" . }}://{{ include "ontele.host" . }}{{ end }}
{{- end }}

{{/* Service the edge (Ingress/HTTPRoute) points at */}}
{{- define "ontele.edgeService" -}}
{{- if .Values.oauth2Proxy.enabled }}{{ include "ontele.component" (dict "root" . "name" "oauth2-proxy") }}{{ else }}{{ include "ontele.fullname" . }}{{ end }}
{{- end }}
{{- define "ontele.edgePort" -}}
{{- if .Values.oauth2Proxy.enabled }}4180{{ else }}{{ .Values.service.port }}{{ end }}
{{- end }}

{{/*
Database
*/}}
{{- define "ontele.postgresql.secretName" -}}
{{- default (include "ontele.component" (dict "root" . "name" "postgresql")) .Values.postgresql.auth.existingSecret }}
{{- end }}

{{/*
Generated secrets: random on first install, then re-read from the existing
Secret on upgrades (lookup), and memoised in .Values so every template that
needs the value in one render sees the same one.
*/}}
{{- define "ontele.generated" -}}
{{- $root := .root }}{{- $key := .key }}{{- $secret := .secret }}{{- $field := .field }}
{{- if not (hasKey $root.Values $key) }}
{{- $value := "" }}
{{- $existing := lookup "v1" "Secret" $root.Release.Namespace $secret }}
{{- if and $existing $existing.data (hasKey $existing.data $field) }}
{{- $value = index $existing.data $field | b64dec }}
{{- else }}
{{- $value = randAlphaNum 32 }}
{{- end }}
{{- $_ := set $root.Values $key $value }}
{{- end }}
{{- index $root.Values $key }}
{{- end }}

{{- define "ontele.postgresql.password" -}}
{{- if .Values.postgresql.auth.password }}{{ .Values.postgresql.auth.password }}{{ else }}{{ include "ontele.generated" (dict "root" . "key" "__pgPassword" "secret" (include "ontele.component" (dict "root" . "name" "postgresql")) "field" "POSTGRES_PASSWORD") }}{{ end }}
{{- end }}

{{- define "ontele.databaseUrl" -}}
{{- if .Values.postgresql.enabled -}}
postgres://{{ .Values.postgresql.auth.username }}:{{ include "ontele.postgresql.password" . }}@{{ include "ontele.component" (dict "root" . "name" "postgresql") }}:5432/{{ .Values.postgresql.auth.database }}
{{- else -}}
{{ .Values.externalDatabase.url }}
{{- end -}}
{{- end }}

{{/* oauth2-proxy */}}
{{- define "ontele.oauth2Proxy.secretName" -}}
{{- default (include "ontele.component" (dict "root" . "name" "oauth2-proxy")) .Values.oauth2Proxy.existingSecret }}
{{- end }}

{{- define "ontele.oauth2Proxy.cookieSecret" -}}
{{- if .Values.oauth2Proxy.cookieSecret }}{{ .Values.oauth2Proxy.cookieSecret }}{{ else }}{{ include "ontele.generated" (dict "root" . "key" "__cookieSecret" "secret" (include "ontele.component" (dict "root" . "name" "oauth2-proxy")) "field" "OAUTH2_PROXY_COOKIE_SECRET") }}{{ end }}
{{- end }}

{{- define "ontele.oauth2Proxy.upstreams" -}}
http://{{ include "ontele.fullname" . }}:{{ .Values.service.port }}/
{{- if .Values.grafana.enabled }},http://{{ include "ontele.component" (dict "root" . "name" "grafana") }}:3000/grafana/{{ end }}
{{- end }}

{{/* Dex issuer as seen by the browser */}}
{{- define "ontele.dex.issuer" -}}
{{ include "ontele.publicUrl" . }}/dex
{{- end }}

{{/*
storageClassName line with global fallback:
  {{ include "ontele.storageClassName" (dict "root" $ "local" .storageClass) }}
Per-volume value wins over global.storageClass; "-" renders an explicit
empty class (static binding); empty renders nothing (cluster default).
*/}}
{{- define "ontele.storageClassName" -}}
{{- $sc := coalesce .local .root.Values.global.storageClass -}}
{{- if $sc }}
{{- if eq $sc "-" }}storageClassName: ""{{ else }}storageClassName: {{ $sc | quote }}{{ end }}
{{- end }}
{{- end }}

{{/* True when this library volume needs a chart-created PVC. */}}
{{- define "ontele.libraryNeedsPvc" -}}
{{- $nfs := .nfs | default dict }}
{{- if and .enabled (not .existingClaim) (not .hostPath) (not $nfs.server) }}true{{ end }}
{{- end }}

{{- define "ontele.imagePullSecrets" -}}
{{- with .Values.imagePullSecrets }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}
