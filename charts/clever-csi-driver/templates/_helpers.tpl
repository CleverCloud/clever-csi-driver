{{- define "csi.labels" -}}
app.kubernetes.io/name: clever-csi-driver
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}
{{- define "csi.image" -}}
{{ printf "%s:%s" .Values.image.repository .Values.image.tag }}
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
