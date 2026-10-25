// Yonder's error body is `{ "error": "<message>" }` — a single field,
// not an RFC 7807 problem document.
export class ApiError extends Error {
  status: number;
  body?: unknown;

  constructor(message: string, status: number, body?: unknown) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }

  static async fromResponse(
    response: Response,
    body: unknown,
  ): Promise<ApiError> {
    const status = response.status;
    let message = response.statusText;
    if (body && typeof body === "object") {
      const problem = body as Record<string, unknown>;
      if (typeof problem.error === "string") message = problem.error;
    }
    return new ApiError(message, status, body);
  }
}
