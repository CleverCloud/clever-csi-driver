{{- define "csi.labels" -}}
app.kubernetes.io/name: clever-csi-driver
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}
{{- define "csi.image" -}}
{{- if .Values.image.digest -}}
{{ printf "%s@%s" .Values.image.repository .Values.image.digest }}
{{- else -}}
{{ printf "%s:%s" .Values.image.repository .Values.image.tag }}
{{- end -}}
{{- end -}}
{{- define "csi.sidecarResources" -}}
{{- $override := index .root.Values.sidecarResourcesByRole .role | default dict -}}
{{- mergeOverwrite (deepCopy .root.Values.sidecarResources) $override | toJson -}}
{{- end -}}
{{- define "csi.probes" -}}
ports:
  - {name: health, containerPort: 9808}
livenessProbe:
  httpGet: {path: /healthz, port: health}
  initialDelaySeconds: 10
  periodSeconds: 10
readinessProbe:
  httpGet: {path: /readyz, port: health}
  periodSeconds: 5
{{- end -}}
